# Consolette ↔ Internal Model Gateway: Authentication Research

**Question:** Can Consolette authenticate to the internal Model Gateway from Tyler's
personal (employer-managed) Mac laptop — off-mesh, not from a deployed service — using a
bearer token or API key?

**Date:** 2026-07-17 · **Researcher:** Consolette research subagent

---

## Summary / Verdict

**There is no real off-mesh bearer token or API key for the Model Gateway. Every
documented client config uses a *dummy* bearer (`sk-dummy`) — the real authentication is
always the internal identity system's mTLS**, injected by one of two things: (a) a locally-running internal proxy
component, or (b) the internal identity CLI (`internal-identity curl`) against a directly-dialable VIP. Confidence: **HIGH.**

Concretely, Consolette has three laptop-viable options, in order of least effort:

| Mode | How | Real auth | Consolette code cost | Confidence |
|------|-----|-----------|----------------------|------------|
| **A. `bearer` → SBN Dev Agent local proxy** (recommended NOW) | Point at `http://localhost:9123/proxy/{PROJECT_ID}`; send dummy bearer | SBN Dev Agent mints internal identity credentials on your behalf | ~zero (plain HTTP + dummy header) | HIGH |
| **B. `bearer` → self-run mesh sidecar** | `newt --app-type mesh start`; point at `copilotdppython-secure…{internal-mesh-vip}:7002/proxy/{PROJECT}` | proxyd sidecar does mTLS | ~zero (plain HTTP) | HIGH |
| **C. `auth = internal` (native mTLS)** (future) | Consolette dials `https://copilotdppython.{internal-service-vip}:7004/v1/…` directly with the laptop's internal identity user cert + `x-internal-project-id` header | mTLS via the internal identity system, done by Consolette (or shelling to the internal identity CLI) | Non-trivial (cert plumbing) | MEDIUM |

**Bottom line for the `auth = bearer` vs `auth = internal` decision:**
- **NOW: implement `bearer` mode = "plain HTTP to a local proxy with a dummy bearer."** This
  is exactly how Claude Code, Codex, Cursor, Goose, Aider, and Zed are wired
  internally today. It is fully documented, laptop-native, and requires almost no code. The word
  "bearer" in Consolette's config is honest only in the mechanical sense — the token is a
  placeholder; identity comes from the local agent.
- **LATER: `auth = internal` is genuinely feasible from a personal Mac** because the laptop
  already has an internal identity system *user* identity and the `…{internal-service-vip}:7004`
  VIP is directly reachable (no mesh sidecar required) — this is precisely what
  `internal-identity curl -a copilotdppython` does. The open question is native-Rust cert plumbing
  (see the mTLS section); the pragmatic bridge is to shell out to the internal identity CLI.

What does **NOT** exist: a personal access token / `go/…`-minted API key you can send to a
public endpoint with no local internal component. Do not design around one.

---

## 1. Base URLs

There are **three distinct address families**. Only two are laptop-reachable.

### Laptop-reachable

1. **SBN Dev Agent local proxy** (the paved road for laptops):
   - `http://mgp.local.dev.{internal-domain}:9123/proxy/{PROJECT_ID}` (alias: `http://localhost:9123/proxy/{PROJECT_ID}`)
   - A newer variant appears in NCP-2914: `http://127.0.0.1:9124/proxy/modelgateway`
   - The agent listens locally and mints internal identity credentials on the developer's behalf.
   - Confidence: HIGH (Model Gateway Local Proxy Setup manual + multiple install docs).

2. **Direct Discovery VIP via the internal identity CLI** (mTLS-direct, no mesh sidecar):
   - `https://copilotdppython.{internal-service-vip}:7004/v1/…`
   - This is a *regular* `.vip.<region>.<env>.cloud.{internal-domain}` VIP — directly dialable from
     a laptop on VPN, unlike the mesh svip below. Reached with
     `internal-identity curl -a copilotdppython …` + `x-internal-project-id` header.
   - Confidence: HIGH that the endpoint + the internal identity CLI works from a laptop (multiple docs
     show `/v1/chat/completions` and Vertex `generateContent` calls this way).

### NOT laptop-reachable directly (mesh-only — needs a proxyd sidecar)

3. **Mesh secure VIP** (data plane; used by deployed services and by the self-run sidecar):
   - `http://copilotdppython-secure.{REGION}.{ENV}.{internal-mesh-vip}:2002` — OpenAI base,
     `/v1/chat/completions` directly.
   - `http://copilotdppython-secure.{REGION}.{ENV}.{internal-mesh-vip}:7002/proxy/{PROJECT}` —
     used for the `/proxy/{PROJECT}` prefix form (OpenAI **and** Anthropic).
   - `.{internal-mesh-vip}` names are only resolvable/reachable *through a co-located mesh
     proxyd sidecar* that terminates/originates mTLS. A laptop reaches this only by running
     `newt --app-type mesh start …` (Option B). The Shimmer manual is explicit: **"Don't talk
     to the Model Gateway data plane directly."**
   - Registration record (ESO Eval doc): `vip: copilotdppython-secure`, `port: 7004`,
     `auth: oidc-envoy`, `appIdentity: copilotdppython`.

> Region/env used in examples: `REGION=us-east-1`, `ENV=prod`.

---

## 2. Endpoint paths

The Model Gateway is a Python FastAPI service (`corp/ncp-copilot-dp-python`, Spinnaker app
`copilotdppython`). Documented routes:

- **OpenAI-compatible:**
  - `POST /v1/chat/completions` (primary)
  - `POST /v1/embeddings`
  - `POST /v1/images/generations`
  - `POST /v1/assistants/*` (full OpenAI Assistants API: threads, messages, runs, files)
- **Anthropic-native — CONFIRMED:**
  - `POST /v1/messages` exists as a **catch-all passthrough proxy straight to Anthropic**.
    Evidence (Jira **NCP-2914**): *"Claude Code CLI → `ANTHROPIC_BASE_URL/v1/messages` (native
    Anthropic) → copilotdppython `/v1/messages` → catch-all proxy → Anthropic … direct
    passthrough to Anthropic."* This is highly relevant: **Claude Code speaks Anthropic
    `/v1/messages` natively, and the gateway supports it directly** — no OpenAI translation
    needed.
  - Client wiring for the Anthropic path:
    ```bash
    export ANTHROPIC_BASE_URL="http://localhost:9123/proxy/{PROJECT_ID}"   # note: NO /v1 suffix
    export ANTHROPIC_API_KEY="sk-dummy"     # or ANTHROPIC_AUTH_TOKEN=sk-dummy
    ```
    (The Anthropic SDK/Claude Code appends `/v1/messages` itself, so the base URL stops at
    `/proxy/{PROJECT_ID}`.)
- **Project selection:** either the `/proxy/{PROJECT_ID}/…` path prefix (local proxy & mesh
  forms) **or** the `x-internal-project-id: {PROJECT_ID}` header (direct-VIP form).
- **Baseten / passthrough:** `/baseten/model/{model_id}/…` for non-chat endpoints.

Confidence: HIGH for both `/v1/chat/completions` and `/v1/messages` existence.

---

## 3. Auth handshake from a laptop

**Finding: The bearer/API key is a placeholder in 100% of documented configs.** Observed
values: `sk-dummy`, `apiKeyHelper: "echo sk-1234"`, `apiKeyHelper: "echo test"`,
`ANTHROPIC_AUTH_TOKEN=sk-dummy`. The Local Proxy Setup manual states plainly: *"Use
`sk-dummy` as the API key — the proxy handles real authentication via the internal identity system."*

Real inbound auth on the gateway is **`auth: oidc-envoy`** (internal-identity-system/mesh-terminated
identity) plus **Gandalf policy** authorization per caller (policy name pattern
`NCP-copilot-prod-{PROJECT_ID}`). There is **no bearer-token or API-key credential path**
that the gateway itself validates.

So "authenticating from a laptop" reduces to *how the caller's internal identity reaches the
gateway*:

- **Via SBN Dev Agent (Option A):** local agent on `:9123` holds/mints internal identity
  credentials and attaches them upstream. Consolette speaks plain HTTP with a dummy bearer. Requires:
  VPN on; project created at `go/modelgateway`; your user in the project's Gandalf policy;
  agent installed (`newt --app-type=java-project install-dev-agent`) and listening
  (`lsof -i :9123`).
- **Via self-run mesh sidecar (Option B):** `newt --app-type mesh start -e prod -s proxy-config.yaml`
  stands up proxyd exposing the `…{internal-mesh-vip}:7002/proxy/{PROJECT}` endpoint;
  Consolette speaks plain HTTP with a dummy bearer.
- **Via the internal identity CLI to the direct VIP (Option C precursor):** the laptop's **internal
  identity system *user* identity** authenticates directly. Example that works from a laptop:
  ```bash
  internal-identity curl -a copilotdppython \
    'https://copilotdppython.{internal-service-vip}:7004/v1/chat/completions' \
    -H 'x-internal-project-id: {PROJECT_ID}' \
    -H 'Content-Type: application/json' \
    -d '{"model":"...","messages":[{"role":"user","content":"hi"}]}'
  ```

**Verdict for §3:** No off-mesh bearer/API-key issuance path exists. The laptop-native
"bearer" experience is a *dummy bearer to a local proxy that injects internal identity credentials*. Confidence: HIGH.

---

## 4. The `auth = internal` (native mTLS) path — later

**Feasibility from a personal Mac: YES, plausibly**, and better than the "mesh-only" fear —
because the `copilotdppython.{internal-service-vip}:7004` VIP is a *directly
dialable* Discovery VIP (not a mesh svip), and the laptop already has an internal identity system user
identity (the same one the internal identity CLI and the in-repo Jira skill use). This means Consolette
could, in principle, do the mTLS leg itself and drop the local-proxy dependency entirely.

What native `auth = internal` in Rust would require:

1. **Client identity = the developer's internal-identity-system user cert + private key.** On a
   employer-managed laptop these are short-lived certs managed by the internal identity agent and
   consumed transparently by the internal identity CLI. Two implementation strategies:
   - **Shell out to the internal identity CLI** for the HTTPS leg (simplest, proven, matches every doc;
     Consolette becomes a thin wrapper). Lowest risk.
   - **Native reqwest + rustls/native-tls** loading the internal-identity-system cert/key and dialing
     `:7004` directly. Cleaner long-term but requires locating the cert/key material and
     handling rotation. **No well-known public Rust crate for this internal identity system was found** — this is the
     main unknown.
2. **Cert rotation handling** — internal-identity-system certs are short-lived; Consolette must re-read or
   re-invoke the internal identity CLI on rotation.
3. **Request shape:** `x-internal-project-id` header + `/v1/chat/completions` (OpenAI)
   or `/v1/messages` (Anthropic). VPN required.

Confidence: HIGH that the direct-VIP + internal identity CLI path works from a laptop today;
MEDIUM on native-Rust cert plumbing (crate/cert-path/rotation details unverified).

**Recommendation:** ship Option A/B `bearer` mode first; treat `auth = internal` as a v2 that
initially shells out to the internal identity CLI against `:7004`, then optionally graduates to native
rustls once the cert-material path is verified.

---

## 5. Rate limits / quotas

From the Model Gateway Proxy FAQ (dated ~April 2025 — **treat as illustrative, verify current
numbers**):

- **Per-model, shared across all internal users**, token-bucket algorithm:
  - Claude 3.7 Sonnet: **1M tokens/min** (shared) — was the tightest; increase to 10M was planned.
  - Claude 3.5: **10M tokens/min**.
  - GPT-4o: **~100M tokens/min**.
  - Gemini 2.5: **~50 requests/day** (very restrictive at the time).
- **Per-project RPM/TPM limits** also enforced by the gateway (gateways.md).
- 429s use a token bucket → back off ~1 min. Feeds Consolette's separate rate-limiting
  workstream: implement per-model cooldown + retry/backoff, and expect *shared* (not
  per-user) budgets on Anthropic models.

Confidence: MEDIUM (numbers are ~15 months old).

---

## 6. Open risks / unverified items

1. **SBN Dev Agent is Java-oriented.** Install is `newt --app-type=java-project
   install-dev-agent`. Confirm it runs cleanly for a non-Java user and survives laptop
   reboots (Option A's hard dependency). Confidence gap: MEDIUM.
2. **Native-Rust mTLS for the internal identity system is unproven.** No public Rust crate for it
   found; cert/key file locations and rotation cadence not verified. Mitigate by shelling to
   the internal identity CLI first. Confidence gap: MEDIUM-HIGH.
3. **Anthropic `/v1/messages` over the *direct VIP* (`:7004`) not explicitly shown.** Docs
   show `/v1/chat/completions` over `:7004` and `/proxy/{PROJECT}/v1/messages` over the mesh
   proxy. The direct-VIP Anthropic path is *likely* (same FastAPI app + catch-all) but
   **unverified** — test before relying on it for Option C. Confidence gap: MEDIUM.
4. **Sourcegraph Deep Search could not be run** — my user needs a separate OAuth grant
   (`sgcreds` authorize URL). Route-list confirmation of `ncp-copilot-dp-python` is therefore
   from docs/Jira, not direct source read. Confidence gap: LOW-MEDIUM.
5. **Everything requires: VPN on + project registered at `go/modelgateway` + user in the
   project's Gandalf policy** (`NCP-copilot-prod-{PROJECT_ID}`). A missing Gandalf membership
   yields 401; wrong/absent project yields 404.
6. **Policy note (compliance):** Internal policy forbids personal OpenAI/Anthropic keys for
   work use — the Model Gateway is the sanctioned path. Consolette must not offer a
   "bring your own personal key to a public vendor" mode for work use.
7. **Ports differ by path form** (`:2002` OpenAI-direct, `:7002` `/proxy` prefix, `:7004`
   direct VIP, `:9123`/`:9124` local agent) — Consolette config must not hardcode one.

---

## Sources

- **Model Gateway Local Proxy Setup** (the canonical laptop guide; shows dummy keys +
  OpenAI/Anthropic base URLs + the internal identity system): https://{internal-manuals-host}/genaiplatform/main/model-gateway-local-proxy-setup.md
- **Agent Platform Gateways** (routes, ports, `copilotdppython`, per-project rate limits):
  https://{internal-manuals-host}/ncpteam/main/concepts/gateways.md
- **Shimmer — AI Gateway** ("SBN Dev Agent mints internal identity credentials"; "don't talk to data plane
  directly"; `Bearer sk-dummy` ignored): https://{internal-manuals-host}/shimmer/main/developer-guide/references/ai-gateway.md/
- **Model Gateway Proxy FAQ** (desktop tools; rate-limit numbers; mesh 503 restart via
  `newt --app-type mesh start`): https://{internal-manuals-host}/genaiplatform/main/model-gateway-proxy.md
- **Model Gateway Overview** (owner = Data Discovery/GenAI, `#genai-platform-help`):
  https://{internal-manuals-host}/genaiplatform/main/model-gateway.md
- **Baseten via Model Gateway** (mesh secure VIP base URL `:2002`, `x-internal-project-id`):
  https://{internal-manuals-host}/genaiplatform/main/model-gateway-baseten.md
- **Codex CLI install** (`base_url = http://mgp.local.dev.{internal-domain}:9123/proxy/<project>/v1`,
  dummy key): https://{internal-manuals-host}/codex-cli-docs/main/getting-started/installation.md/
- **Matti GenAI — Python/Java** (direct VIP `internal-identity curl -a copilotdppython
  https://copilotdppython.{internal-service-vip}:7004/v1/chat/completions`):
  https://{internal-manuals-host}/matti-genai/main/llms/calling-llms/python.md/
- **Jira NCP-2914** (confirms `/v1/messages` native Anthropic catch-all passthrough;
  `ANTHROPIC_BASE_URL=http://127.0.0.1:9124/proxy/modelgateway`):
  https://{internal-jira-host}/browse/NCP-2914
- **SMA-MLVFX / DiCE-MLVFX LLM docs** (`ANTHROPIC_BASE_URL=…{internal-mesh-vip}:7002/proxy/{PROJECT}`,
  `ANTHROPIC_AUTH_TOKEN=sk-dummy`, self-run mesh `proxy-config.yaml`):
  https://{internal-manuals-host}/sma-mlvfx/main/LLM.md/ ·
  https://{internal-manuals-host-alt}/view/dice-mlvfx/mkdocs/main/LLM/
- **ESO Eval Dataset** (gateway registration: `auth: oidc-envoy`, `vip: copilotdppython-secure`,
  `port: 7004`): google doc id `1rpq9h3VVcoXt7tWVjHrZZ09LYbK6giLHdA4sLWjMNDc`
- **GenAI Platform Model Gateway Code Examples** (direct-VIP internal identity CLI REST form):
  google doc id `10j21oaVewmwhIrfvfjFqMOVa-8fW8Nyp8mLQDhEGZUE`
- **Basic Claude Code Install (Coda)** / **How to Install Claude Code** (`~/.claude.json`
  `apiKeyHelper`+`ANTHROPIC_BASE_URL` pattern): coda `_dkYLek3qyP2/_suceMTVW` ·
  google doc id `1Ih9PpCNgrx8_pVlBJLUfmJC8JCQX4sCq2pHIPEzhHBw`
- **SPA Guidance — Agent Sandboxes and Gateways** (no personal vendor keys; use Model
  Gateway): https://{internal-manuals-host}/spa-guidance/main/concepts/genai/agent-sandboxes-and-gateways.md/
- **Existing proxy** (`~/dotfiles/stapler-scripts/claude-proxy-rs/src/auth.rs`,`config.rs`):
  today it only does Bearer→Anthropic / Bedrock; it has **no** Model Gateway target — the
  gateway integration is net-new for Consolette.

*Not obtained:* Sourcegraph Deep Search of `corp/ncp-copilot-dp-python` (blocked on
`sgcreds` OAuth authorization for my user).
