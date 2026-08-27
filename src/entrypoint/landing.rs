//! `GET /` — a discoverability landing page listing every feature
//! consolette actually exposes today: the HTTP endpoints served by this
//! process, and the CLI subcommands available from the `consolette` binary.
//!
//! Deliberately excludes legacy modules ported from the old Python/Rust
//! proxy (`dashboard`, `mcp_gateway`, `memory`, `system_prompt`) that
//! aren't wired into anything reachable yet — listing them here would
//! claim capabilities that don't exist. When one of those gets wired up,
//! add it to `HTTP_ENDPOINTS` or `CLI_COMMANDS` below.

use axum::extract::State;
use axum::response::{Html, IntoResponse};

use super::EntrypointState;

struct Endpoint {
    method: &'static str,
    path: &'static str,
    description: &'static str,
}

/// Every HTTP route this server serves, including this landing page
/// itself. Keep in sync with `entrypoint_router` in `mod.rs`.
const HTTP_ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        method: "GET",
        path: "/",
        description: "This page.",
    },
    Endpoint {
        method: "POST",
        path: "/v1/messages",
        description: "Anthropic-native Messages API. Point ANTHROPIC_BASE_URL here.",
    },
    Endpoint {
        method: "POST",
        path: "/v1/chat/completions",
        description: "OpenAI-compatible Chat Completions API.",
    },
];

struct CliCommand {
    name: &'static str,
    description: &'static str,
}

/// Every `consolette` CLI subcommand. Keep in sync with the `Command` enum
/// in `src/main.rs` — these aren't reachable over HTTP, so this list is the
/// only place they're discoverable from the landing page.
const CLI_COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "run",
        description: "Start this HTTP server (what's currently running).",
    },
    CliCommand {
        name: "list-sessions",
        description: "List Claude Code session transcripts under ~/.claude/projects.",
    },
    CliCommand {
        name: "mcp",
        description: "Serve as an MCP server over stdio (session-compaction tools).",
    },
    CliCommand {
        name: "compact-session",
        description: "Compact a session transcript into a new resumable file.",
    },
    CliCommand {
        name: "serve-cost",
        description: "Serve the cost-metrics HTTP API on loopback.",
    },
    CliCommand {
        name: "cost-report",
        description: "Print a session's cost report from a running serve-cost.",
    },
    CliCommand {
        name: "compare-cost",
        description: "Compare estimated cost of compaction vs. never compacting.",
    },
    CliCommand {
        name: "list-models",
        description: "Query every configured upstream for its available models.",
    },
    CliCommand {
        name: "install",
        description: "Install/update the macOS LaunchAgent that runs `consolette run`.",
    },
];

/// `GET /` — see module docs.
// Kept `async` for signature symmetry with the other Axum handlers.
#[allow(clippy::unused_async)]
pub async fn get_index(State(state): State<EntrypointState>) -> impl IntoResponse {
    Html(render(&state))
}

fn render(state: &EntrypointState) -> String {
    use std::fmt::Write as _;

    let info = &state.server_info;

    let mut upstream_rows = String::new();
    for u in &info.upstreams {
        let _ = write!(
            upstream_rows,
            "<tr><td>{}</td><td>{}</td></tr>",
            escape(&u.name),
            escape(u.kind)
        );
    }

    let mut endpoint_rows = String::new();
    for e in HTTP_ENDPOINTS {
        let _ = write!(
            endpoint_rows,
            "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
            e.method, e.path, e.description
        );
    }

    let mut cli_rows = String::new();
    for c in CLI_COMMANDS {
        let _ = write!(
            cli_rows,
            "<tr><td><code>consolette {}</code></td><td>{}</td></tr>",
            c.name, c.description
        );
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>consolette</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; max-width: 780px; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }}
  h1 {{ margin-bottom: 0.2rem; }}
  .subtitle {{ opacity: 0.7; margin-top: 0; }}
  table {{ border-collapse: collapse; width: 100%; margin: 0.5rem 0 1.5rem; }}
  th, td {{ text-align: left; padding: 0.35rem 0.6rem; border-bottom: 1px solid rgba(128,128,128,0.3); }}
  th {{ opacity: 0.7; font-weight: 600; font-size: 0.85rem; text-transform: uppercase; }}
  code {{ font-size: 0.9em; }}
  section {{ margin-bottom: 2rem; }}
</style>
</head>
<body>
<h1>consolette</h1>
<p class="subtitle">Provider-agnostic LLM router — listening on <code>127.0.0.1:{port}</code></p>

<section>
<h2>Active route</h2>
<p><strong>{route_name}</strong> ({strategy} strategy)</p>
<table>
<thead><tr><th>Upstream</th><th>Kind</th></tr></thead>
<tbody>{upstream_rows}</tbody>
</table>
</section>

<section>
<h2>HTTP endpoints</h2>
<table>
<thead><tr><th>Method</th><th>Path</th><th>Description</th></tr></thead>
<tbody>{endpoint_rows}</tbody>
</table>
</section>

<section>
<h2>CLI commands</h2>
<p>Run these from a terminal — they aren't reachable over HTTP.</p>
<table>
<thead><tr><th>Command</th><th>Description</th></tr></thead>
<tbody>{cli_rows}</tbody>
</table>
</section>

<section>
<h2>Config</h2>
<p>Config lives in <code>~/.config/consolette/conf.d/*.toml</code>, merged in filename order.</p>
</section>
</body>
</html>
"#,
        port = info.port,
        route_name = escape(&info.route_name),
        strategy = escape(&info.strategy),
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entrypoint::UpstreamSummary;

    async fn state_with(upstreams: Vec<UpstreamSummary>) -> EntrypointState {
        use crate::routing::health::HealthRegistry;
        use crate::routing::router::Router as DispatchRouter;
        use crate::routing::strategy::FallbackStrategy;

        EntrypointState {
            dispatch_router: std::sync::Arc::new(DispatchRouter::new(
                vec![],
                vec![],
                std::sync::Arc::new(FallbackStrategy),
                std::sync::Arc::new(HealthRegistry::new(300)),
                std::sync::Arc::new(crate::ratelimit::RateLimiters::new(
                    &crate::config::schema::RateLimitConfig::default(),
                )),
            )),
            cost_tracker: std::sync::Arc::new(
                crate::cost_metrics::tracker::CostTracker::new(
                    crate::cost_metrics::pricing::PricingTable::load_default(),
                )
                .await,
            ),
            server_info: std::sync::Arc::new(super::super::ServerInfo {
                port: 47000,
                route_name: "default".to_string(),
                strategy: "Fallback".to_string(),
                upstreams,
            }),
        }
    }

    #[tokio::test]
    async fn render_lists_every_configured_upstream() {
        let state = state_with(vec![
            UpstreamSummary {
                name: "anthropic".to_string(),
                kind: "anthropic",
            },
            UpstreamSummary {
                name: "model-gateway-openai".to_string(),
                kind: "openai",
            },
        ])
        .await;

        let html = render(&state);
        assert!(html.contains("anthropic"));
        assert!(html.contains("model-gateway-openai"));
        assert!(html.contains("127.0.0.1:47000"));
    }

    #[tokio::test]
    async fn render_lists_http_endpoints_and_cli_commands() {
        let state = state_with(vec![]).await;
        let html = render(&state);
        assert!(html.contains("/v1/messages"));
        assert!(html.contains("/v1/chat/completions"));
        assert!(html.contains("consolette install"));
        assert!(html.contains("consolette list-models"));
    }

    #[test]
    fn escape_handles_special_chars() {
        assert_eq!(escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
