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
