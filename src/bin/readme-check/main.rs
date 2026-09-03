//! CI gate keeping `README.md` honest against the actual `consolette` CLI
//! and config schema, so a README example can't silently drift from the
//! code it documents (the failure mode this binary exists to catch: a
//! subcommand gets renamed/added/removed, or the sample TOML config stops
//! parsing, and nobody notices until a user copy-pastes it).
//!
//! Checks two things:
//! 1. Every row in README.md's "## CLI reference" table (`` `consolette
//!    <name>` ``) matches exactly the subcommand list clap actually
//!    generates for the `consolette` binary (via `--help`).
//! 2. The first `toml` fenced code block under "## Configuration"
//!    deserializes against the real [`consolette::config::schema`] types.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

fn readme_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md")
}

/// Subcommand names documented in the "## CLI reference" table, extracted
/// from table rows shaped like `` | `consolette <name> ...` | ... | ``.
fn documented_commands(readme: &str) -> BTreeSet<String> {
    readme
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with('|') {
                return None;
            }
            let after_tick = line.split('`').nth(1)?;
            let mut parts = after_tick.split_whitespace();
            if parts.next()? != "consolette" {
                return None;
            }
            parts.next().map(str::to_string)
        })
        .collect()
}

/// Subcommand names clap actually registers, parsed from `consolette
/// --help`'s "Commands:" section. Skips clap's auto-generated `help`.
fn real_commands() -> anyhow::Result<BTreeSet<String>> {
    let output = Command::new("cargo")
        .args(["run", "--quiet", "--bin", "consolette", "--", "--help"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "`cargo run --bin consolette -- --help` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout)?;

    let mut commands = BTreeSet::new();
    let mut in_commands_section = false;
    for line in stdout.lines() {
        if line.trim() == "Commands:" {
            in_commands_section = true;
            continue;
        }
        if !in_commands_section {
            continue;
        }
        if line.trim().is_empty() || line.trim() == "Options:" {
            break;
        }
        if let Some(name) = line.split_whitespace().next() {
            if name != "help" {
                commands.insert(name.to_string());
            }
        }
    }
    Ok(commands)
}

/// The first `toml` fenced code block under the "## Configuration" heading.
fn config_example(readme: &str) -> Option<&str> {
    let after_heading = readme.split("## Configuration").nth(1)?;
    let after_open = after_heading.split("```toml").nth(1)?;
    after_open.split("```").next()
}

#[derive(serde::Deserialize)]
struct ReadmeConfigExample {
    #[serde(default)]
    upstreams: Vec<consolette::config::schema::Upstream>,
    #[serde(default)]
    routes: Vec<consolette::config::schema::Route>,
}

fn main() -> anyhow::Result<()> {
    let readme = std::fs::read_to_string(readme_path())?;
    let mut failed = false;

    let documented = documented_commands(&readme);
    let real = real_commands()?;

    let missing_from_readme: Vec<_> = real.difference(&documented).collect();
    let stale_in_readme: Vec<_> = documented.difference(&real).collect();

    if !missing_from_readme.is_empty() {
        failed = true;
        eprintln!("README.md's CLI reference is missing: {missing_from_readme:?}");
    }
    if !stale_in_readme.is_empty() {
        failed = true;
        eprintln!("README.md's CLI reference documents commands that no longer exist: {stale_in_readme:?}");
    }
    if !failed {
        println!(
            "README.md's CLI reference matches `consolette --help` ({} commands).",
            real.len()
        );
    }

    if let Some(toml) = config_example(&readme) {
        match toml::from_str::<ReadmeConfigExample>(toml) {
            Ok(example) => println!(
                "README.md's config example parses ({} upstream(s), {} route(s)).",
                example.upstreams.len(),
                example.routes.len()
            ),
            Err(e) => {
                failed = true;
                eprintln!(
                    "README.md's config example under \"## Configuration\" failed to parse: {e}"
                );
            }
        }
    } else {
        failed = true;
        eprintln!("README.md has no `toml` example under \"## Configuration\" to check");
    }

    if failed {
        anyhow::bail!("README.md has drifted from the code it documents");
    }
    Ok(())
}
