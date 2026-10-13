//! Fails when the embedded Angular bundle is a placeholder or older than any input `build.rs`
//! watches, so a green Rust fixture test cannot ship with a stale UI. Run via `make ui-fresh-check`.
//! Skipped under `CONSOLETTE_SKIP_UI` (the developer opted out of building the UI).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const BUILD_RS: &str = include_str!("../build.rs");

/// Quoted strings inside `const <name>: ...= ...;` of `build.rs`, so this test can never drift
/// from the list `build.rs` actually watches.
fn build_rs_strings(name: &str) -> Vec<String> {
    let start = BUILD_RS
        .find(&format!("const {name}"))
        .unwrap_or_else(|| panic!("build.rs has no const {name}"));
    let decl = &BUILD_RS[start..];
    let end = decl.find(';').expect("const ends with ;");
    decl[..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let own = fs::metadata(path).and_then(|m| m.modified()).ok()?;
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

/// Reasons the bundle under `root` is stale; empty when it is fresh.
fn stale_reasons(root: &Path, inputs: &[String], browser_dir: &str) -> Vec<String> {
    let index = root.join(browser_dir).join("index.html");
    let Ok(html) = fs::read_to_string(&index) else {
        return vec![format!("{} is missing", index.display())];
    };
    if !html.contains("<app-root") {
        return vec![format!("{} is the placeholder page", index.display())];
    }
    let built = fs::metadata(&index).and_then(|m| m.modified()).unwrap();
    inputs
        .iter()
        .filter(|p| newest_mtime(&root.join(p)).is_some_and(|t| t > built))
        .map(|p| format!("stale input `{p}` is newer than {}", index.display()))
        .collect()
}

fn write_index(root: &Path, html: &str) -> PathBuf {
    let dir = root.join("dist");
    fs::create_dir_all(&dir).unwrap();
    let index = dir.join("index.html");
    fs::write(&index, html).unwrap();
    index
}

#[test]
fn embedded_ui_bundle_should_not_be_older_than_any_build_rs_input_or_be_the_placeholder() {
    if std::env::var("CONSOLETTE_SKIP_UI").is_ok_and(|v| !v.is_empty() && v != "0") {
        eprintln!("CONSOLETTE_SKIP_UI set; skipping UI freshness check");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = build_rs_strings("UI_INPUTS");
    let browser_dir = &build_rs_strings("BROWSER_DIR")[0];
    assert!(inputs.len() >= 5, "failed to parse UI_INPUTS from build.rs");

    let reasons = stale_reasons(root, &inputs, browser_dir);

    assert!(
        reasons.is_empty(),
        "embedded UI is stale; rebuild with CONSOLETTE_REQUIRE_UI=1 cargo build:\n{}",
        reasons.join("\n")
    );
}

#[test]
fn stale_check_should_name_the_input_that_is_newer_than_the_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let index = write_index(tmp.path(), "<app-root></app-root>");
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    let input = tmp.path().join("src/main.ts");
    fs::write(&input, "x").unwrap();
    let built = fs::metadata(&index).unwrap().modified().unwrap();
    let newer = built + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_modified(newer)
        .unwrap();

    let reasons = stale_reasons(tmp.path(), &["src".to_string()], "dist");

    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("`src`"), "{reasons:?}");
}

#[test]
fn stale_check_should_flag_the_placeholder_page_and_pass_a_fresh_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    write_index(tmp.path(), "<h1>Consolette Dashboard (Loading...)</h1>");
    let reasons = stale_reasons(tmp.path(), &[], "dist");
    assert!(reasons[0].contains("placeholder"), "{reasons:?}");

    write_index(tmp.path(), "<app-root></app-root>");
    assert!(stale_reasons(tmp.path(), &[], "dist").is_empty());
}
