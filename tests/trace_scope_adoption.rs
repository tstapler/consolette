//! Mechanical check that every request-ingress site is inside a function that
//! owns or receives a `TraceScope` (Story 1.1.3, Task 1.1.3j; pre-mortem P1-2).
//!
//! Scans `src/**/*.rs` with `std::fs` for three kinds of site:
//! - `Router::dispatch` calls (a `.dispatch(` call with four arguments, which
//!   tells it apart from the one-argument MCP/emulation `dispatch` methods),
//! - `dispatch_traced(` calls,
//! - `CostTrackingStream::new(` calls.
//!
//! Each site must sit in a function whose signature or body mentions
//! `TraceScope`, unless it is on a literal allowlist below. Code under
//! `#[cfg(test)] mod ...`, `src/bin/` and `tests` directories is ignored.
//!
//! Allowlists: the handlers adopted their scope in Story 2.1.6 (wrapper plus inner
//! function; `post_v1_messages` and `post_v1_chat_completions` now mention
//! `TraceScope` themselves). What remains is permanent. `COMPAT_WRAPPER` is the
//! `Router::dispatch` wrapper that begins and finishes its own scope.
//! `SCOPE_HOLDERS` are helpers that reach the scope through `self` or are only
//! called from a function that owns it. A stale entry (no longer matching any
//! scope-less site) fails the test, so the lists cannot rot.

use std::fs;
use std::path::{Path, PathBuf};

/// (file, enclosing fn): helpers without `TraceScope` in their text that still run inside
/// a scope owned by the handler wrapper.
const SCOPE_HOLDERS: &[(&str, &str)] = &[
    ("src/entrypoint/messages/mod.rs", "dispatch"),
    ("src/entrypoint/messages/mod.rs", "direct_stream_response"),
    ("src/entrypoint/messages/mod.rs", "stream_response"),
];

/// (file, enclosing fn): the compat wrapper that begins and finishes its own scope.
const COMPAT_WRAPPER: &[(&str, &str)] = &[("src/routing/router/mod.rs", "dispatch")];

#[derive(Debug, PartialEq, Eq)]
struct Site {
    file: String,
    line: usize,
    enclosing_fn: String,
    has_scope: bool,
}

fn fn_name(trimmed: &str) -> Option<&str> {
    let rest = trimmed.split("fn ").nth(1)?;
    let prefix = trimmed.split("fn ").next()?;
    let modifiers_only = prefix
        .split_whitespace()
        .all(|w| w == "pub" || w == "async" || w == "const" || w.starts_with("pub("));
    if !modifiers_only {
        return None;
    }
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    Some(&rest[..end])
}

/// Index of the first line of the trailing `#[cfg(test)] mod ...` block, if any.
fn test_module_start(lines: &[&str]) -> usize {
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let next_item = lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.starts_with("#[") && !l.is_empty());
        if next_item.is_some_and(|l| l.starts_with("mod ") || l.starts_with("pub mod ")) {
            return i;
        }
    }
    lines.len()
}

/// Number of top-level arguments in the call whose `(` is at `open` in `text`.
fn arg_count(text: &str, open: usize) -> usize {
    let mut depth = 0usize;
    let mut args = 0usize;
    let mut saw_token = false;
    for c in text[open..].chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return args + usize::from(saw_token);
                }
            }
            ',' if depth == 1 => {
                args += 1;
                saw_token = false;
            }
            c if depth >= 1 && !c.is_whitespace() => saw_token = true,
            _ => {}
        }
    }
    0
}

fn find_sites(file: &str, source: &str) -> Vec<Site> {
    let lines: Vec<&str> = source.lines().collect();
    let end = test_module_start(&lines);
    let prod = lines[..end].join("\n");
    let mut sites = Vec::new();
    for (i, line) in lines[..end].iter().enumerate() {
        let line_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let mut hits: Vec<usize> = Vec::new();
        for needle in ["dispatch_traced(", "CostTrackingStream::new("] {
            for (pos, _) in line.match_indices(needle) {
                hits.push(pos + needle.len() - 1);
            }
        }
        for (pos, _) in line.match_indices(".dispatch(") {
            let open = pos + ".dispatch".len();
            if arg_count(&prod, line_start + open) == 4 {
                hits.push(open);
            }
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        for _ in hits {
            sites.push(site_at(file, &lines, i));
        }
    }
    sites
}

fn site_at(file: &str, lines: &[&str], line_idx: usize) -> Site {
    let start = (0..=line_idx)
        .rev()
        .find(|&j| fn_name(lines[j].trim_start()).is_some());
    let (enclosing_fn, has_scope) = match start {
        Some(j) => {
            let indent = lines[j].len() - lines[j].trim_start().len();
            let closer = format!("{}}}", " ".repeat(indent));
            let stop = (j..lines.len())
                .find(|&k| lines[k] == closer)
                .unwrap_or(lines.len() - 1);
            let name = fn_name(lines[j].trim_start()).unwrap_or("?").to_string();
            (
                name,
                lines[j..=stop].iter().any(|l| l.contains("TraceScope")),
            )
        }
        None => ("<module level>".to_string(), false),
    };
    Site {
        file: file.to_string(),
        line: line_idx + 1,
        enclosing_fn,
        has_scope,
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "bin" && name != "tests" {
                rust_files(&path, out)?;
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            out.push(path);
        }
    }
    Ok(())
}

fn scan_src() -> Result<Vec<Site>, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files).map_err(|e| e.to_string())?;
    files.sort();
    let mut sites = Vec::new();
    for path in files {
        let source = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        sites.extend(find_sites(&rel, &source));
    }
    Ok(sites)
}

fn listed(list: &[(&str, &str)], site: &Site) -> bool {
    list.iter()
        .any(|(f, n)| *f == site.file && *n == site.enclosing_fn)
}

#[test]
fn every_dispatch_caller_and_cost_tracking_stream_new_should_be_inside_a_trace_scope_owning_wrapper_or_on_the_allowlist(
) -> Result<(), String> {
    let sites = scan_src()?;
    assert!(
        !sites.is_empty(),
        "scanner found no ingress sites at all; the scan is broken, not the code"
    );

    let violations: Vec<String> = sites
        .iter()
        .filter(|s| !s.has_scope && !listed(COMPAT_WRAPPER, s) && !listed(SCOPE_HOLDERS, s))
        .map(|s| {
            format!(
                "{}:{} in fn `{}` dispatches or builds a CostTrackingStream without a TraceScope",
                s.file, s.line, s.enclosing_fn
            )
        })
        .collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));

    let stale: Vec<String> = SCOPE_HOLDERS
        .iter()
        .filter(|(f, n)| {
            !sites
                .iter()
                .any(|s| s.file == *f && s.enclosing_fn == *n && !s.has_scope)
        })
        .map(|(f, n)| format!("{f} `{n}`: allowlisted but no longer a scope-less site; remove it"))
        .collect();
    assert!(stale.is_empty(), "{}", stale.join("\n"));
    Ok(())
}

#[test]
fn scanner_should_flag_scope_less_sites_and_pass_scoped_or_test_only_ones() {
    let source = "\
async fn bad(router: &R) {
    router
        .dispatch(a, b, c, d)
        .await;
}

async fn good(scope: &mut TraceScope, router: &R) {
    router.dispatch(a, b, c, d).await;
    let t = CostTrackingStream::new(x);
}

fn one_arg(s: &S) {
    s.dispatch(request);
}

fn nested(router: &R) {
    router.dispatch(f(a, b), g(c), d, e).await;
}

#[cfg(test)]
mod tests {
    async fn t(router: &R) {
        router.dispatch(a, b, c, d).await;
    }
}
";
    let sites = find_sites("x.rs", source);
    let summary: Vec<(&str, usize, bool)> = sites
        .iter()
        .map(|s| (s.enclosing_fn.as_str(), s.line, s.has_scope))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("bad", 3, false),
            ("good", 8, true),
            ("good", 9, true),
            ("nested", 17, false),
        ]
    );
}
