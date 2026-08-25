use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};

use consolette::claude_code_session::discovery::{discover_sessions, SortBy};
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
    /// Locate Claude Code session transcripts under
    /// `~/.claude/projects/**/*.jsonl` and print them sorted.
    ListSessions {
        /// Sort order: `recent` (default), `oldest`, `largest`, `smallest`.
        #[arg(long, default_value = "recent")]
        sort: String,
        /// Maximum number of sessions to print.
        #[arg(long)]
        limit: Option<usize>,
    },
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
    /// Compare the estimated cost of never compacting a transcript against
    /// any real compaction metrics already stamped into it. Both sides are
    /// `TiktokenEstimator` + `PricingTable` estimates, not live-measured
    /// costs from the `claude` CLI — the no-compaction figure also ignores
    /// real prompt caching, since it assumes every turn resends the full
    /// growing transcript.
    CompareCost {
        /// Path to the session's `.jsonl` transcript.
        session: PathBuf,
        /// Pricing model to estimate costs against.
        #[arg(long, default_value = consolette::claude_code_session::DEFAULT_PRICING_MODEL)]
        pricing_model: String,
    },
    /// Install consolette's context-forensics hooks into
    /// `~/.claude/settings.json`, additively and idempotently.
    ContextTrackerUp,
    /// Remove exactly the hook entries `context-tracker up` installed,
    /// leaving every other entry (including ones added afterward) intact.
    ContextTrackerDown,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::Run => run().await,
        Command::ListSessions { sort, limit } => list_sessions_command(&sort, limit),
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
        Command::CompareCost {
            session,
            pricing_model,
        } => compare_cost_command(&session, &pricing_model).await,
        Command::ContextTrackerUp => context_tracker_up_command(),
        Command::ContextTrackerDown => context_tracker_down_command(),
    }
}

fn context_tracker_up_command() -> anyhow::Result<()> {
    let path = consolette::context_forensics::hooks_install::SettingsJsonGateway::default_path();
    consolette::context_forensics::hooks_install::up(&path).with_context(|| {
        format!(
            "failed to install context-forensics hooks into {}",
            path.display()
        )
    })?;
    println!("installed context-forensics hooks into {}", path.display());
    Ok(())
}

fn context_tracker_down_command() -> anyhow::Result<()> {
    let path = consolette::context_forensics::hooks_install::SettingsJsonGateway::default_path();
    consolette::context_forensics::hooks_install::down(&path).with_context(|| {
        format!(
            "failed to remove context-forensics hooks from {}",
            path.display()
        )
    })?;
    println!("removed context-forensics hooks from {}", path.display());
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let config = config::load(&config_dir())?;
    println!(
        "consolette: loaded config (port {}, {} upstream(s), {} route(s))",
        config.port,
        config.upstreams.len(),
        config.routes.len()
    );
    let state = consolette::entrypoint::EntrypointState::build(&config).await?;
    consolette::entrypoint::serve_entrypoint(config.port, state).await
}

fn list_sessions_command(sort: &str, limit: Option<usize>) -> anyhow::Result<()> {
    let sort_by = SortBy::parse(sort)?;
    let mut sessions = discover_sessions(sort_by)?;
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }

    if sessions.is_empty() {
        println!("no session transcripts found under ~/.claude/projects/**/*.jsonl");
        return Ok(());
    }

    for session in &sessions {
        let age = session.modified.elapsed().map_or_else(
            |_| "unknown age".to_string(),
            |d| format!("{}s ago", d.as_secs()),
        );
        println!(
            "{}\t{:>10} bytes\t{}",
            session.path.display(),
            session.size_bytes,
            age
        );
    }
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

/// Handler for `consolette compare-cost`: prints the naive no-compaction
/// cost estimate for a transcript side-by-side with any real compaction
/// metrics already stamped into it by `compact-session`.
async fn compare_cost_command(
    session: &std::path::Path,
    pricing_model: &str,
) -> anyhow::Result<()> {
    let comparison = consolette::claude_code_session::cost_compare::compare_compaction_cost(
        session,
        pricing_model,
    )
    .await
    .with_context(|| {
        format!(
            "failed to compare compaction cost for {}",
            session.display()
        )
    })?;

    println!("consolette compare-cost: {}", session.display());
    println!(
        "  note: figures below are TiktokenEstimator + PricingTable ESTIMATES, not live-measured"
    );
    println!("        costs from the `claude` CLI, and the no-compaction total ignores real");
    println!("        prompt caching (it assumes every turn resends the full growing transcript).");
    println!();
    let coverage = comparison.chain_coverage;
    if coverage.chain_messages < coverage.total_messages {
        println!(
            "⚠ warning: only {}/{} messages ({:.0}%) in this transcript are on the active \
             conversation chain — the rest are earlier, disconnected conversation history \
             (e.g. from prior `--resume`/`--clear` cycles) that this estimate does NOT cover.",
            coverage.chain_messages,
            coverage.total_messages,
            coverage.ratio() * 100.0
        );
        println!(
            "  the figures below only reflect the most recent conversation thread in this file."
        );
        println!();
    }

    println!("No-compaction counterfactual (pricing model: {pricing_model}):");
    println!(
        "  total tokens (cumulative, growing-context): {}",
        comparison.no_compaction.total_tokens
    );
    match comparison.no_compaction.estimated_cost_usd {
        Some(cost) => println!("  estimated cost: ${cost:.4}"),
        None => println!("  estimated cost: unknown (no pricing data for {pricing_model})"),
    }
    println!();

    if !comparison.is_compacted {
        println!("Compaction metrics: none found (transcript has not been compacted)");
    } else if comparison.compaction_metrics.is_empty() {
        println!(
            "Compaction metrics: transcript has a compaction boundary, but no turns were \
             summarized (all turns were preserved verbatim)"
        );
    } else {
        println!(
            "Compaction metrics ({} compaction event(s) found):",
            comparison.compaction_metrics.len()
        );
        for (i, metrics) in comparison.compaction_metrics.iter().enumerate() {
            println!("  [{i}] tokens before: {}", metrics.tokens_before);
            println!("  [{i}] tokens after:  {}", metrics.tokens_after);
            println!("  [{i}] tokens saved:  {}", metrics.tokens_saved);
            match metrics.estimated_cost_usd {
                Some(cost) => println!("  [{i}] estimated cost: ${cost:.4}"),
                None => println!("  [{i}] estimated cost: unknown"),
            }
            match metrics.real_cost_usd {
                Some(cost) => println!("  [{i}] real cost (claude CLI-reported): ${cost:.4}"),
                None => println!(
                    "  [{i}] real cost: unknown (summarizer didn't report one, or transcript predates this field)"
                ),
            }
        }
    }

    Ok(())
}
