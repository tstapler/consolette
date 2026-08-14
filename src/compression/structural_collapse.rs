//! Structural collapsing: merges consecutive import statements into a single
//! summary line, and collapses runs of lines that share a structural
//! template (repeated assertions, repeated log entries with only a varying
//! number/string) down to their first and last occurrence plus a count.
//!
//! Never touches identifiers — only lines that are entirely boilerplate
//! (import statements) or that are template-identical apart from numbers and
//! quoted-string contents are collapsed, and the first/last line of a
//! collapsed run is always kept verbatim.

use std::sync::LazyLock;

use regex::Regex;

const MIN_RUN: usize = 3;

#[allow(clippy::expect_used)]
static PY_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*import\s+(\S+?)(?:\s+as\s+\S+)?\s*$").expect("PY_IMPORT regex")
});

#[allow(clippy::expect_used)]
static PY_FROM_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*from\s+(\S+)\s+import\s+.+$").expect("PY_FROM_IMPORT regex"));

#[allow(clippy::expect_used)]
static JS_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*import\s+.+\s+from\s+['"]([^'"]+)['"];?\s*$"#).expect("JS_IMPORT regex")
});

#[allow(clippy::expect_used)]
static JS_REQUIRE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:const|let|var)\s+\S+\s*=\s*require\(['"]([^'"]+)['"]\);?\s*$"#)
        .expect("JS_REQUIRE regex")
});

#[allow(clippy::expect_used)]
static JAVA_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*import\s+(?:static\s+)?([\w.]+);\s*$").expect("JAVA_IMPORT regex")
});

#[allow(clippy::expect_used)]
static TEMPLATE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("TEMPLATE_NUMBER regex"));

#[allow(clippy::expect_used)]
static TEMPLATE_SINGLE_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"'[^']*'").expect("TEMPLATE_SINGLE_QUOTED regex"));

#[allow(clippy::expect_used)]
static TEMPLATE_DOUBLE_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*""#).expect("TEMPLATE_DOUBLE_QUOTED regex"));

/// If `line` is a recognized Python/JS/TS/Java import statement, return the
/// short name (module path, import source, or fully-qualified class) that
/// identifies it in a merged summary.
fn import_name(line: &str) -> Option<String> {
    // Check the semicolon-/quote-terminated forms (Java, JS/TS) before the
    // bare `import <token>` Python form, which would otherwise also match
    // those lines and swallow the trailing punctuation into its capture.
    if let Some(caps) = JAVA_IMPORT.captures(line) {
        let full = &caps[1];
        return Some(full.rsplit('.').next().unwrap_or(full).to_string());
    }
    if let Some(caps) = JS_IMPORT.captures(line) {
        return Some(caps[1].to_string());
    }
    if let Some(caps) = JS_REQUIRE.captures(line) {
        return Some(caps[1].to_string());
    }
    if let Some(caps) = PY_FROM_IMPORT.captures(line) {
        return Some(caps[1].to_string());
    }
    if let Some(caps) = PY_IMPORT.captures(line) {
        return Some(caps[1].to_string());
    }
    None
}

/// Merge runs of `MIN_RUN`+ consecutive import statements into a single
/// `[imports: a,b,c]` summary line. Returns the input unchanged if no run
/// meets the threshold, or if merging wouldn't shrink the text.
#[must_use]
pub fn collapse_import_blocks(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let names: Vec<Option<String>> = lines.iter().map(|&l| import_name(l)).collect();

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if names[i].is_none() {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && names[j].is_some() {
            j += 1;
        }
        let run_len = j - i;
        if run_len >= MIN_RUN {
            let merged = names[i..j]
                .iter()
                .filter_map(Option::as_ref)
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            out.push(format!("[imports: {merged}]"));
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }

    let result = out.join("\n");
    if result.len() < text.len() {
        result
    } else {
        text.to_string()
    }
}

/// Normalize a line into a structural template by blanking out digit runs
/// and quoted-string contents, so lines that differ only in a counter,
/// timestamp, or literal string compare equal.
fn line_template(line: &str) -> String {
    let s = TEMPLATE_NUMBER.replace_all(line, "#");
    let s = TEMPLATE_SINGLE_QUOTED.replace_all(&s, "''");
    let s = TEMPLATE_DOUBLE_QUOTED.replace_all(&s, "\"\"");
    s.into_owned()
}

/// Collapse runs of `MIN_RUN`+ consecutive lines that share a structural
/// template (e.g. repeated assertions or log entries varying only in a
/// number or literal string) down to the first line, a count of the elided
/// middle lines, and the last line. Never rewrites a kept line, so
/// identifiers are always left intact. Returns the input unchanged if no run
/// meets the threshold, or if collapsing wouldn't shrink the text.
#[must_use]
pub fn collapse_repeated_templates(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() {
        return text.to_string();
    }
    let templates: Vec<String> = lines.iter().map(|&l| line_template(l)).collect();

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && templates[j] == templates[i] && !lines[j].trim().is_empty() {
            j += 1;
        }
        let run_len = j - i;
        if run_len >= MIN_RUN {
            out.push(lines[i].to_string());
            out.push(format!("[{} more lines like this]", run_len - 2));
            out.push(lines[j - 1].to_string());
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }

    let result = out.join("\n");
    if result.len() < text.len() {
        result
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_python_import_block() {
        let text = "import os\nimport sys\nfrom typing import List\nprint('hi')";
        let result = collapse_import_blocks(text);
        assert!(result.contains("[imports: os,sys,typing]"));
        assert!(result.contains("print('hi')"));
    }

    #[test]
    fn merges_java_import_block() {
        let text =
            "import java.util.List;\nimport java.util.Map;\nimport java.io.File;\nclass Foo {}";
        let result = collapse_import_blocks(text);
        assert!(result.contains("[imports: List,Map,File]"));
    }

    #[test]
    fn merges_js_import_block() {
        let text = "import foo from 'foo-lib';\nimport bar from 'bar-lib';\nconst baz = require('baz-lib');\nconsole.log('go');";
        let result = collapse_import_blocks(text);
        assert!(result.contains("[imports: foo-lib,bar-lib,baz-lib]"));
    }

    #[test]
    fn does_not_merge_below_threshold() {
        let text = "import os\nimport sys\nprint('hi')";
        assert_eq!(collapse_import_blocks(text), text);
    }

    #[test]
    fn never_touches_non_import_identifiers() {
        let text = "import os\nimport sys\nimport json\nresult = os.getcwd()";
        let result = collapse_import_blocks(text);
        assert!(result.contains("result = os.getcwd()"));
    }

    #[test]
    fn collapses_repeated_assertions() {
        let text = "assert foo(1) == 'a'\nassert foo(2) == 'b'\nassert foo(3) == 'c'\nassert foo(4) == 'd'\ndone";
        let result = collapse_repeated_templates(text);
        assert!(result.contains("assert foo(1) == 'a'"));
        assert!(result.contains("[2 more lines like this]"));
        assert!(result.contains("assert foo(4) == 'd'"));
        assert!(result.contains("done"));
    }

    #[test]
    fn does_not_collapse_below_threshold() {
        let text = "assert foo(1) == 'a'\nassert foo(2) == 'b'\ndone";
        assert_eq!(collapse_repeated_templates(text), text);
    }

    #[test]
    fn does_not_collapse_dissimilar_lines() {
        let text = "alpha\nbeta\ngamma\ndelta";
        assert_eq!(collapse_repeated_templates(text), text);
    }

    #[test]
    fn blank_lines_break_a_run() {
        let text = "log entry 1\nlog entry 2\n\nlog entry 3\nlog entry 4";
        assert_eq!(collapse_repeated_templates(text), text);
    }
}
