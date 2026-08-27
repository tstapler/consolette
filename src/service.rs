//! Install/update the macOS `LaunchAgent` that runs `consolette run` in the
//! background (ADR-005 Story 6.3: label `com.consolette`, port 47000,
//! `RunAtLoad=false`, `KeepAlive=true`).
//!
//! Deviation from ADR-005's literal contract: the ADR's `ProgramArguments`
//! is `[<consolette binary path>]` with no subcommand. The current CLI
//! (`src/main.rs`) has no default subcommand — `consolette` alone errors
//! requiring one — so `ProgramArguments` here is `[<bin>, "run"]` to match
//! what the binary actually requires.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context};

const LABEL: &str = "com.consolette";

/// Renders and writes `~/Library/LaunchAgents/com.consolette.plist`, then
/// reloads it via `launchctl bootout`/`bootstrap` so an already-running
/// agent picks up the change. `start` additionally kicks the service so it
/// begins running immediately, rather than waiting for the next login.
///
/// # Errors
///
/// Returns an error on non-macOS platforms, if `HOME`/the current binary
/// path can't be resolved, if the plist can't be written, or if the
/// `launchctl`/`id` subprocesses fail.
pub fn install(start: bool) -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`consolette install` only supports macOS LaunchAgents right now");
    }

    let bin = std::env::current_exe().context("failed to resolve consolette's own binary path")?;
    let home = std::env::var("HOME").context("HOME is not set")?;
    let launch_agents_dir = PathBuf::from(&home).join("Library").join("LaunchAgents");
    let plist_path = launch_agents_dir.join(format!("{LABEL}.plist"));

    std::fs::create_dir_all(&launch_agents_dir)
        .context("failed to create ~/Library/LaunchAgents")?;
    std::fs::write(&plist_path, render_plist(&bin, &home))
        .with_context(|| format!("failed to write {}", plist_path.display()))?;
    println!("wrote {}", plist_path.display());

    let uid = current_uid()?;
    let domain_target = format!("gui/{uid}/{LABEL}");

    // Best-effort: fails if the agent isn't currently loaded, which is fine
    // on a first install.
    let _ = Command::new("launchctl")
        .args(["bootout", &domain_target])
        .status();

    let status = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.to_string_lossy(),
        ])
        .status()
        .context("failed to run `launchctl bootstrap`")?;
    if !status.success() {
        bail!("`launchctl bootstrap` exited with {status}");
    }
    println!("loaded {LABEL} (RunAtLoad=false — starts on next login, or pass --start now)");

    if start {
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &domain_target])
            .status()
            .context("failed to run `launchctl kickstart`")?;
        if !status.success() {
            bail!("`launchctl kickstart` exited with {status}");
        }
        println!("started {LABEL}");
    }

    Ok(())
}

fn current_uid() -> anyhow::Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run `id -u`")?;
    if !output.status.success() {
        bail!("`id -u` exited with {}", output.status);
    }
    Ok(String::from_utf8(output.stdout)
        .context("`id -u` printed non-UTF8 output")?
        .trim()
        .to_string())
}

fn render_plist(bin: &std::path::Path, home: &str) -> String {
    let bin = bin.display();
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());

    // Carried over from whatever launched `consolette install`, matching the
    // legacy plist's env set (ADR-005) — conf.d is the preferred home for
    // everything else, so nothing else is forwarded here.
    let mut extra_env = String::new();
    for var in [
        "AWS_PROFILE",
        "AWS_REGION",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CONSOLETTE_PORT",
    ] {
        if let Ok(value) = std::env::var(var) {
            let _ = writeln!(
                extra_env,
                "        <key>{var}</key>\n        <string>{}</string>",
                xml_escape(&value)
            );
        }
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home}</string>
        <key>PATH</key>
        <string>{path_env}</string>
{extra_env}    </dict>
    <key>RunAtLoad</key>
    <false/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>/tmp/consolette.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/consolette.err.log</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_plist_includes_run_subcommand_and_label() {
        let plist = render_plist(
            std::path::Path::new("/usr/local/bin/consolette"),
            "/Users/tstapler",
        );
        assert!(plist.contains("<string>com.consolette</string>"));
        assert!(plist.contains("<string>/usr/local/bin/consolette</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(
            plist.contains("<false/>"),
            "RunAtLoad must be false per ADR-005"
        );
        assert!(
            plist.contains("<true/>"),
            "KeepAlive must be true per ADR-005"
        );
    }

    #[test]
    fn xml_escape_handles_special_chars() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
