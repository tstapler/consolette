//! Builds the Angular dashboard that `src/dashboard.rs` embeds via `rust-embed`.
//!
//! `ui/dist` is gitignored, so the UI is an input cargo can't see on its own; without this a
//! stale or missing bundle is silently embedded. Env knobs:
//! - `CONSOLETTE_SKIP_UI=1`: never run npm (editors/`cargo check`); keeps any existing bundle.
//! - `CONSOLETTE_REQUIRE_UI=1`: fail instead of falling back to a placeholder page (CI/release).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

const UI_DIR: &str = "ui";
const BROWSER_DIR: &str = "ui/dist/consolette/browser";
const PLACEHOLDER_HTML: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"UTF-8\"><title>Consolette Dashboard</title></head>\n<body><h1>Consolette Dashboard (Loading...)</h1></body>\n</html>";

/// Paths whose changes require a UI rebuild.
const UI_INPUTS: &[&str] = &[
    "ui/src",
    "ui/public",
    "ui/package.json",
    "ui/package-lock.json",
    "ui/angular.json",
    "ui/tsconfig.json",
    "ui/tsconfig.app.json",
];

fn main() {
    for input in UI_INPUTS.iter().chain(&[BROWSER_DIR]) {
        println!("cargo:rerun-if-changed={input}");
    }
    println!("cargo:rerun-if-env-changed=CONSOLETTE_SKIP_UI");
    println!("cargo:rerun-if-env-changed=CONSOLETTE_REQUIRE_UI");

    let index = Path::new(BROWSER_DIR).join("index.html");
    let skip = env_flag("CONSOLETTE_SKIP_UI");
    let require = env_flag("CONSOLETTE_REQUIRE_UI");

    if !skip && ui_is_stale(&index) {
        if let Err(err) = build_ui() {
            if require {
                panic!("CONSOLETTE_REQUIRE_UI is set but the UI build failed: {err}");
            }
            println!("cargo:warning=dashboard UI not built ({err}); keeping any existing bundle, else a placeholder page");
        }
    }

    if !index.exists() {
        let _ = fs::create_dir_all(BROWSER_DIR);
        let _ = fs::write(&index, PLACEHOLDER_HTML);
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Stale when there is no real bundle (missing, or the placeholder) or any input is newer than
/// the bundle's `index.html`, which `ng build` rewrites on every run.
fn ui_is_stale(index: &Path) -> bool {
    let Ok(html) = fs::read_to_string(index) else {
        return true;
    };
    if !html.contains("<app-root") {
        return true;
    }
    let Some(built) = mtime(index) else {
        return true;
    };
    UI_INPUTS
        .iter()
        .filter_map(|p| newest_mtime(Path::new(p)))
        .any(|t| t > built)
}

fn build_ui() -> Result<(), String> {
    // npm writes node_modules/.package-lock.json on install; older than the lockfile = stale.
    let installed = mtime(Path::new("ui/node_modules/.package-lock.json"));
    let lock = mtime(Path::new("ui/package-lock.json"));
    if installed.is_none() || installed < lock {
        run_npm(&["ci"])?;
    }
    run_npm(&["run", "build"])
}

fn run_npm(args: &[&str]) -> Result<(), String> {
    let status = Command::new("npm")
        .args(args)
        .current_dir(UI_DIR)
        .status()
        .map_err(|e| format!("could not run npm: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`npm {}` exited with {status}", args.join(" ")))
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Newest mtime of `path` or, for a directory, anything beneath it.
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let own = mtime(path)?;
    let Ok(entries) = fs::read_dir(path) else {
        return Some(own);
    };
    Some(
        entries
            .flatten()
            .filter_map(|e| newest_mtime(&e.path()))
            .fold(own, SystemTime::max),
    )
}
