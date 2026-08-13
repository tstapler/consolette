use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod auth;
mod config;
mod providers;
mod ratelimit;
mod routing;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::Run => run(),
        Command::Mcp => mcp(),
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

fn mcp() -> anyhow::Result<()> {
    // Wire up an rmcp server over stdio here — see
    // https://github.com/modelcontextprotocol/rust-sdk for the current API.
    anyhow::bail!("MCP server not yet implemented");
}
