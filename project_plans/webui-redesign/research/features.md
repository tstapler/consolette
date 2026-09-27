# Research: Best-in-Class LLM Proxy & Observability Dashboard Features

**Date**: 2026-09-27  
**Project**: Consolette Web UI Redesign (`project_plans/webui-redesign`)  
**Target Architecture**: Single-binary Rust (`Axum 0.8`) + Embedded Angular SPA  

---

## 1. Executive Summary & Industry Landscape

To transition Consolette's web UI from a static single-page string renderer to an enterprise-grade LLM proxy control center, we surveyed 5 leading open-source and commercial LLM observability and proxy platforms:

| Platform | Primary Focus | Standout Strengths |
| :--- | :--- | :--- |
| **LiteLLM UI** | Proxy Management & Routing | Dynamic load balancing, provider key management, budget/rate limits. |
| **Portkey Dashboard** | Gateway & Fallbacks | Multi-provider fallback pipelines, retry orchestration, guardrails. |
| **Helicone UI** | Real-time Observability & Costs | Deep token breakdown, session replay, prompt versioning, sub-second analytics. |
| **Langfuse** | LLM Engineering & Tracing | Step-by-step trace tree, prompt diffing, dataset benchmarking. |
| **OpenRouter Dashboard** | Multi-Provider Analytics | Live model latency percentiles (p50/p90/p99), global throughput, live uptime matrix. |

---

## 2. Feature Analysis by Core Pillar

### Pillar 1: Live Request & Metric Monitoring

Modern LLM proxy dashboards require high-frequency, low-latency metrics visualization to monitor request flows, cost velocity, and provider health.

#### Best-in-Class Capabilities
1. **Real-Time KPI Cards with Sparklines**:
   - **Requests Per Minute (RPM)** & **Tokens Per Second (TPS)** with rolling 1-minute, 15-minute, and 1-hour trend indicators.
   - **Time-to-First-Token (TTFT)**: Real-time median and 95th percentile TTFT gauges updated stream-by-stream.
   - **Cost Estimation**: Live running cost calculator broken down by model pricing tiers (prompt vs completion token rates).
   - **Token Compression & Savings Counter**: Cumulative tokens saved and percentage reduction ratio (e.g. *74.2% token savings via prompt compression*).

2. **Provider Health & Cooldown Status Badges**:
   - Visual status indicators for each upstream provider target (`Anthropic`, `AWS Bedrock`, `OpenAI`).
   - States: `Healthy (Green)`, `Degraded (Yellow)`, `In Cooldown (Orange - 429 Rate Limit)` with live countdown timer (e.g. `Cooldown: 14s remaining`), `Auth Error (Red - Invalid/Expired Key)`.
   - Automatic visual flashing or badge shift when a provider enters cooldown or triggers an error cascade.

3. **Live SSE/WebSocket Stream Feed**:
   - Rolling request event feed showing active, streaming, completed, and failed requests.
   - Inline status tags (`200 OK`, `429 Rate Limit`, `500 Provider Error`, `504 Timeout`).
   - Micro-meters for input tokens, output tokens, TTFT, and total duration per request.

---

### Pillar 2: Session Replay & Payload Diff Inspection

Debugging LLM proxy behaviors, prompt compression algorithms, and agent session loops requires detailed payload inspection and visual diffing.

#### Best-in-Class Capabilities
1. **Side-by-Side Payload Diff Viewer**:
   - **Original Prompt vs Compressed/Modified Prompt**: Split-pane or unified git-style visual diff inspector highlighting added, modified, or truncated text blocks.
   - Character and token delta metrics directly above the diff editor (e.g. *Original: 4,120 tokens → Processed: 1,050 tokens (-74.5%)*).
   - Syntax-highlighted JSON viewer for system prompts, messages array, tool declarations, and completion parameters (`temperature`, `top_p`).

2. **Granular Token Breakdown**:
   - Visual breakdown bars segmenting:
     - **Input / System Tokens**
     - **Context / History Tokens**
     - **Compressed / Filtered Tokens Saved**
     - **Output / Generation Tokens**
     - **Cached Tokens** (Provider-level prompt caching hits).

3. **Trace Tree & Pipeline Execution Steps**:
   - Chronological step timeline for each request lifecycle:
     `Proxy Ingress` → `Auth & Rate Limit Check` → `Prompt Compression Layer` → `Upstream Router` → `Provider Dispatch (Attempt 1: Anthropic -> 429)` → `Fallback Trigger` → `Provider Dispatch (Attempt 2: Bedrock -> 200 OK)` → `Stream Egress`.
   - Detailed timing and latency attribution per pipeline stage.

4. **Interactive Sandbox / Request Replay**:
   - "Replay Request" action button: Copies the exact request payload into an interactive playground pane where operators can modify prompt text or parameters and resend through the proxy to observe diffs in real time.

---

### Pillar 3: Live Model Benchmarking & Latency Percentiles

Operators need continuous evaluation of model performance and upstream provider SLAs to make data-driven routing decisions.

#### Best-in-Class Capabilities
1. **Latency Percentile Distribution (p50, p90, p95, p99)**:
   - Separate percentiles for **TTFT (Time To First Token)** and **Total Execution Duration**.
   - Multi-line area charts comparing provider/model combinations (e.g. `claude-3-5-sonnet@anthropic` vs `claude-3-5-sonnet@bedrock`).
   - CDF (Cumulative Distribution Function) graphs to surface long-tail latency spikes under load.

2. **Upstream Error & Fallback Analytics**:
   - Error rate percentage matrix categorizing error types: `HTTP 429 (Rate Limit)`, `HTTP 401/403 (Auth Failure)`, `HTTP 5xx (Upstream Outage)`, `Context Length Exceeded`, `Timeout`.
   - Fallback event counter tracking how often primary models fail and fall back to secondary providers.

3. **Comparative Model Benchmark Table**:
   - Sortable table comparing active models across key performance indicators:
     - Median TTFT (ms)
     - Generation Speed (tokens/sec)
     - Success Rate (%)
     - Total Cost / 1k requests ($)
     - Average Token Savings (%)

---

### Pillar 4: Interactive Proxy Configuration Editor

Moving beyond read-only dashboards, an effective proxy management interface provides visual controls for dynamic routing, fallbacks, and rate limits.

#### Best-in-Class Capabilities
1. **Visual Load Balancing & Routing Weights**:
   - Drag-and-drop or interactive percentage sliders for distributing request volume across providers/regions (e.g., Anthropic Direct: 70%, AWS Bedrock: 30%).
   - Rule-based conditional routing builder (e.g., *If model = `claude-3-5-sonnet` AND prompt_length > 10k tokens → route to Bedrock*).

2. **Fallback Cascade & Retry Builder**:
   - Interactive pipeline cascade diagram for configuring failover paths:
     `Primary Model` → `On 429 / 5xx / TTFT > 2500ms` → `Fallback Model 1` → `Fallback Model 2`.
   - Configurable retry counts, exponential backoff delays, and circuit breaker trip conditions.

3. **Rate Limit & Quota Rules**:
   - UI forms for defining rate limit policies: RPM (Requests Per Minute), TPM (Tokens Per Minute), Concurrent Request limits, and Budget Caps ($ per key/hour).
   - Real-time rate limit utilization progress bars showing proximity to quota thresholds.

4. **Config Validation, Visual Diff & Hot-Reloading**:
   - Schema validation with inline error highlighting prior to applying config changes.
   - "Compare with Active Config" modal showing a YAML/JSON diff of changes.
   - Single-click **Apply Config (Hot Reload)** calling Axum backend REST endpoints without requiring binary restart.

---

## 3. Consolette Web UI Recommendations & Implementation Plan

### UI Component Architecture (Angular Standalone Components + Signals)
- **Dashboard Layout**: Responsive sidebar navigation with core sections:
  1. `/overview` — Live Metrics, Provider Health Cards, SSE Stream Feed.
  2. `/sessions` — Session Replay, Trace Tree, Side-by-Side Payload Diff.
  3. `/benchmarks` — Latency Percentiles (p50/p90/p99), TTFT Charts, Error Matrix.
  4. `/config` — Interactive Routing Weight Sliders, Fallback Cascade Builder, Hot Reload.

- **Data Layer Integration**:
  - SSE client service (`SseService`) streaming `/v1/dashboard/events` directly into Angular `Signals` (`signal()`, `computed()`).
  - Rolling buffer capped at 500 recent events in memory to prevent memory growth during prolonged sessions.

- **Zero-Dependency Charting**:
  - Bundle `Chart.js` / `ngx-charts` directly into the Angular application build.
  - Zero external CDN fetching, ensuring 100% offline air-gapped binary execution.

---

## 4. Summary Matrix

| Feature Module | Key UI Element | Primary Metric / Data | User Value |
| :--- | :--- | :--- | :--- |
| **Live Monitoring** | Status Cards & Badges | RPM, TTFT, Token Savings %, Cooldown timers | Instant visibility into proxy health & savings |
| **Session Replay** | Split Diff Viewer & Trace Tree | Original vs Compressed prompt, Token breakdown | Debug prompt compression & request routing |
| **Benchmarking** | Percentile Area Charts | p50/p90/p99 TTFT, Provider Error Rates | Optimize provider selection & SLA tracking |
| **Config Editor** | Routing Sliders & Cascade Builder | Weights, Fallback triggers, Rate limit rules | Live tuning without restarting Rust proxy |
