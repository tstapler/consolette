# consolette

A provider-agnostic LLM router: point `ANTHROPIC_BASE_URL` (or an OpenAI-compatible client) at it, and it proxies requests to Anthropic, AWS Bedrock, or any OpenAI-compatible upstream — with automatic fallback, weighted routing, rate limiting, and cost tracking, all configurable without restarting.

Ships as a single Rust binary with two companion tools for the Claude Code / MCP ecosystem: `mcp-proxy` (shrinks MCP server tool-schema payloads to save context) and `cmdcrush` (compresses command output before it reaches an LLM's context).

## Install

```bash
brew install tstapler/tap/consolette

# or from source
cargo install --path .
```

## Quick start

No config required — a fresh install already knows how to route to Anthropic with Bedrock as fallback:

```bash
consolette run
```

This starts the HTTP proxy on `127.0.0.1:47000`, exposing:

- `POST /v1/messages` — Anthropic-native Messages API
- `POST /v1/chat/completions` — OpenAI-compatible Chat Completions API
- `GET /dashboard` — live-updating monitoring dashboard
- `GET /` — a landing page listing every route this instance serves

Point a client at it:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:47000
```

Or, for a client that speaks the OpenAI Chat Completions API instead — e.g.
[OpenCode](https://opencode.ai) — add a custom provider to `opencode.json`
pointed at consolette's `/v1/chat/completions` (no `apiKey` is checked;
consolette doesn't authenticate loopback requests):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "consolette": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Consolette",
      "options": {
        "baseURL": "http://127.0.0.1:47000/v1",
        "apiKey": "unused"
      },
      "models": {
        "claude-sonnet-4-5": {
          "name": "Claude Sonnet 4.5 (via consolette)"
        }
      }
    }
  }
}
```

## Configuration

Config lives in `~/.config/consolette/conf.d/*.toml`, merged in filename order (`00-`, `10-`, … — later files win). An empty `conf.d` still works: it reproduces the built-in Anthropic-primary/Bedrock-fallback default.

To add another upstream or change routing strategy, drop in a file like:

```toml
# ~/.config/consolette/conf.d/10-routing.toml
[[upstreams]]
name = "my-openai-upstream"
kind = "openai"
base_url = "https://api.example.com"

[[routes]]
name = "default"
strategy = "weighted"

[[routes.upstreams]]
name = "anthropic"
weight = 2.0

[[routes.upstreams]]
name = "my-openai-upstream"
weight = 1.0
```

Once running, the same route can be inspected and changed live from the web control panel:

- `GET /api/models` — every configured upstream's live model catalog
- `GET /api/route` — the active route (strategy, upstream weights, model overrides)
- `POST /api/route` — replace the active route; persists to `~/.config/consolette/runtime-overrides.toml` and applies immediately, no restart

## CLI reference

| Command | Purpose |
|---|---|
| `consolette run` | Start the HTTP proxy (above). |
| `consolette install [--start]` | Install/update the macOS `LaunchAgent` so `run` survives login and crashes. |
| `consolette list-models` | Query every configured upstream for its available models. |
| `consolette mcp` | Serve as an MCP server over stdio (Claude Code session-compaction tools). |
| `consolette list-sessions` | List Claude Code session transcripts under `~/.claude/projects`. |
| `consolette compact-session <path>` | Compact a session transcript into a new resumable file. |
| `consolette serve-cost` | Serve the cost-metrics HTTP API on loopback. |
| `consolette cost-report <session>` | Print a session's cost report from a running `serve-cost`. |
| `consolette compare-cost <session>` | Compare estimated cost of compaction vs. never compacting. |
| `consolette context-tracker-up` | Install consolette's context-forensics hooks in `~/.claude/settings.json`. |
| `consolette context-tracker-down` | Remove exactly the hook entries `context-tracker-up` installed. |
| `consolette context-hook <event>` | Internal: reads a Claude Code hook payload from stdin. Invoked by the installed hooks, not meant to be run directly. |

Run any subcommand with `--help` for its full flags.

### mcp-proxy

An MCP context filter proxy that shrinks `tools/list` schema payloads (truncating descriptions, stripping verbose JSON Schema fields) to reduce the token footprint of MCP servers with large tool catalogs.

### cmdcrush

Runs a command and compresses its captured stdout/stderr before the output reaches an LLM's context, using the same text compressor consolette applies at the `/v1/messages` boundary.

## Development

```bash
lefthook install   # activate pre-commit/pre-push hooks
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contribution workflow, and [RELEASE.md](RELEASE.md) for how versions are cut and published.

## Security

See [SECURITY.md](SECURITY.md) to report a vulnerability.

## License

MIT — see [LICENSE](LICENSE).
