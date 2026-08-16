use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};

use consolette::claude_code_session::mcp_server::CompactionMcpServer;
use consolette::claude_code_session::omission_cache::OmissionCache;
use consolette::claude_code_session::summarize::ClaudeCliSummarizer;
use consolette::config;

/// consolette — CLI / MCP tool.
#[derive(Parser)]
#[command(name = "consolette", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the tool's primary CLI behavior.
    Run,
    /// Serve as an MCP server over stdio.
    Mcp,
    /// Compact a Claude Code session transcript, writing a new resumable
    /// session file alongside the source.
    CompactSession {
        /// Path to the source session's `.jsonl` transcript.
        session: PathBuf,
        /// Number of most-recent turns to keep verbatim, never summarized.
        #[arg(long)]
        preserve_last_n_turns: Option<usize>,
    },
    /// Serve the cost-metrics HTTP API (`GET /v1/cost/{session_key}`) on
    /// loopback, owning the one live `SessionCompactionPipeline` +
    /// `CostTracker` this process shares between them.
    ServeCost {
        /// Overrides `[cost_metrics].port` in the config file and the
        /// built-in default.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Fetch and print a session's cost report from a running
    /// `consolette serve-cost` — a thin HTTP client of that process's
    /// `GET /v1/cost/{session_key}` route, never an independent computation
    /// (plan.md Epic 3.1, repair iteration 1).
    CostReport {
        /// Session key to look up.
        session: String,
        /// Print the raw JSON response instead of the text table.
        #[arg(long)]
        json: bool,
        /// Base URL of the `serve-cost` server. Defaults to
        /// `http://127.0.0.1:<port>`, resolved the same way
        /// `serve-cost --port` resolves its own bind port
        /// (`--server` > `[cost_metrics].port` in the config file > the
        /// built-in default).
        #[arg(long)]
        server: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::Run => run(),
        Command::Mcp => mcp().await,
        Command::CompactSession {
            session,
            preserve_last_n_turns,
        } => compact_session_command(&session, preserve_last_n_turns.unwrap_or(0)).await,
        Command::ServeCost { port } => serve_cost_command(port).await,
        Command::CostReport {
            session,
            json,
            server,
        } => cost_report_command(&session, json, server).await,
    }
}

fn run() -> anyhow::Result<()> {
    let config = config::load(&config_dir())?;
    println!(
        "consolette: loaded config (port {}, {} upstream(s), {} route(s))",
        config.port,
        config.upstreams.len(),
        config.routes.len()
    );
    Ok(())
}

/// `~/.config/consolette`, honoring `HOME` — the loader's own `config_dir`
/// default field is a display value, not the path it was loaded from.
fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("consolette")
}

async fn mcp() -> anyhow::Result<()> {
    use rmcp::{transport::io::stdio, ServiceExt};

    let cache = OmissionCache::open(&OmissionCache::default_cache_path())
        .context("failed to open omission cache")?;
    let server = CompactionMcpServer::new(Arc::new(cache));

    let transport = stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

/// Handler for `consolette compact-session`: parses, plans, prunes,
/// summarizes (via the real `claude` CLI subprocess), and writes a new
/// resumable destination transcript alongside the source.
async fn compact_session_command(
    session: &std::path::Path,
    preserve_last_n_turns: usize,
) -> anyhow::Result<()> {
    let cache = OmissionCache::open(&OmissionCache::default_cache_path())
        .context("failed to open omission cache")?;
    let summarizer = ClaudeCliSummarizer::new(None);

    let parent_dir = session
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let out_path = parent_dir.join(format!("{}.jsonl", uuid::Uuid::new_v4()));

    let new_session_id = consolette::claude_code_session::compact_session(
        session,
        &out_path,
        &cache,
        &summarizer,
        preserve_last_n_turns,
    )
    .await
    .with_context(|| format!("failed to compact session {}", session.display()))?;

    println!("Resume with: claude --resume {new_session_id}");
    Ok(())
}

/// Handler for `consolette serve-cost`: resolves the port (`--port` >
/// `[cost_metrics].port` in the config file > the built-in default) and
/// serves the cost HTTP API forever.
async fn serve_cost_command(port_override: Option<u16>) -> anyhow::Result<()> {
    let port = if let Some(port) = port_override {
        port
    } else {
        let config = config::load(&config_dir())?;
        config
            .cost_metrics
            .port
            .unwrap_or(consolette::cost_metrics::server::DEFAULT_PORT)
    };
    consolette::cost_metrics::server::serve_cost(port).await
}

/// Handler for `consolette cost-report`: resolves the server base URL
/// (`--server` > `[cost_metrics].port` in the config file > the built-in
/// default, mirroring `serve_cost_command`'s own resolution so the flag is
/// optional in the common case), fetches the session's `CostReport` over
/// HTTP, and renders it.
///
/// Exits non-zero via `std::process::exit` (rather than returning `Err`) on
/// a client error, so stderr reads exactly the ux.md-specified message —
/// `main`'s default `anyhow::Result` error formatting would otherwise
/// prepend its own `Error: ` wrapper.
async fn cost_report_command(
    session: &str,
    json: bool,
    server_override: Option<String>,
) -> anyhow::Result<()> {
    let server = if let Some(server) = server_override {
        server
    } else {
        let config = config::load(&config_dir())?;
        let port = config
            .cost_metrics
            .port
            .unwrap_or(consolette::cost_metrics::server::DEFAULT_PORT);
        format!("http://127.0.0.1:{port}")
    };

    match consolette::cost_metrics::client::fetch_cost_report(&server, session).await {
        Ok(report) => {
            if json {
                println!(
                    "{}",
                    consolette::cost_metrics::cli_format::format_cost_report_json(&report)
                );
            } else {
                print!(
                    "{}",
                    consolette::cost_metrics::cli_format::format_cost_report_table(&report)
                );
            }
            Ok(())
        }
        Err(consolette::cost_metrics::client::CostClientError::NotFound { session_key }) => {
            eprintln!("error: no session found for key {session_key:?}");
            std::process::exit(1);
        }
        Err(consolette::cost_metrics::client::CostClientError::Unreachable(_)) => {
            eprintln!(
                "error: could not reach cost server at {server}, is `consolette serve-cost` running?"
            );
            std::process::exit(1);
        }
        Err(consolette::cost_metrics::client::CostClientError::Other(err)) => {
            eprintln!("error: cost server request failed: {err}");
            std::process::exit(1);
        }
    }
}
