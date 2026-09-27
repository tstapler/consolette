# ADR-001: Syntax & Header Variance Telemetry Store and Inspector

**Status**: Accepted
**Date**: 2026-09-26
**Project**: syntax-variance-telemetry

## Context

Clients connecting to Consolette (such as Claude Code or Stapler Squad) frequently send new or experimental HTTP headers (e.g. `anthropic-beta: thinking-2025-02-19`), request body syntax (e.g. `thinking` budgets, `cache_control` markers, `structured_outputs`), or receive non-standard response finish reasons from third-party provider backends (e.g. OpenRouter / Gemini / Bedrock). Currently, when Consolette translates requests between Anthropic and OpenAI/Gemini/Bedrock shapes, unmapped fields or unknown headers are dropped silently, making it difficult to know which new API features clients are attempting to use or which provider response shapes are drifting.

## Decision

We decide to implement a **Syntax & Header Variance Telemetry System** directly in Consolette.
Specifically:
1. **Request & Response Inspection**: Add a lightweight variance inspector module (`src/syntax_variance/`) that inspects incoming HTTP request headers, top-level Anthropic JSON request body keys, and outgoing/provider response shapes against a known schema registry.
2. **Persistence**: Store detected syntax variances in Consolette's existing SQLite database (`~/.claude/consolette/context-forensics.sqlite`) in a new table `syntax_variances` (`variance_type`, `key_name`, `sample_json`, `count`, `first_seen_at`, `last_seen_at`).
3. **Deduplication**: Aggregate recurring variances in SQLite by `(variance_type, key_name)` to prevent database bloat and excessive I/O while tracking frequency and recency.
4. **Observability**: Expose the variance registry to MCP clients via a new `get_syntax_variances` tool in `src/syntax_variance/mcp_server.rs` and integrate with Consolette's stdio MCP server.

## Alternatives Considered

- **Logging Only (stdout/stderr)**: Outputting warnings whenever an unmapped header or key is found. Rejected because verbose logging pollutes terminal output, is ephemeral, and does not aggregate counts or make variances queryable by AI coding tools/MCP servers.
- **Separate SQLite Database File**: Creating a separate `~/.claude/consolette/variances.sqlite` file. Rejected because Consolette already maintains `context-forensics.sqlite` with robust WAL permissions and migration handling (`ContextForensicsStore`), making schema extension cleaner and simpler.

## Rationale

Storing schema drift and unmapped header/body syntax in SQLite with deduplication gives developer tooling and AI agents (via MCP) instant visibility into what client features (e.g., new Anthropic betas) or provider response fields need translation support, without adding overhead to high-throughput streaming requests.

## Consequences

**Positive:**
- Immediate visibility into client beta headers and unmapped request/response syntax.
- Deduplicated persistence ensures minimal SQLite write overhead.
- Directly queryable via MCP (`get_syntax_variances`).

**Negative / Risks:**
- Inspecting JSON body keys adds a negligible (~microseconds) check on incoming request parsing.
