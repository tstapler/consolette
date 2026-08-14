//! Common-prefix collapsing for path-heavy output (`grep -r`, `find`,
//! `ls -laR`): when most lines share a long leading directory path, print
//! that prefix once and strip it from every matching line instead of
//! repeating it on every row.
//!
//! Also covers the rest of claw-compactor's RLE ("repeated leading element")
//! stage family: IP-address shared-prefix aliasing and comma-separated
//! uppercase enum-list compaction.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

const MIN_LINES: usize = 5;
const MIN_PREFIX_LEN: usize = 8;
const MIN_MATCH_RATIO: f64 = 0.6;

/// Matches a dotted-quad IPv4 address.
#[allow(clippy::expect_used)]
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b").expect("IPV4 regex")
});

/// Matches a run of 3+ comma-separated `SCREAMING_SNAKE_CASE` tokens (an
/// enum-style value list), e.g. `FEATURE, BUGFIX, HOTFIX, CHORE`.
#[allow(clippy::expect_used)]
static ENUM_LIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][A-Z0-9_]*(?:,\s+[A-Z][A-Z0-9_]*){2,}\b").expect("ENUM_LIST regex")
});

/// Heuristic: is `text` dominated by lines carrying filesystem paths?
fn path_bearing_lines(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains('/') && !l.trim().is_empty())
        .map(|(i, _)| i)
        .collect()
}

/// Longest common prefix (ending on a `/` boundary) shared by `lines` at the
/// given indices.
fn longest_common_dir_prefix<'a>(lines: &[&'a str], indices: &[usize]) -> &'a str {
    if indices.is_empty() {
        return "";
    }
    let mut prefix = lines[indices[0]];
    for &idx in &indices[1..] {
        let line = lines[idx];
        let mut common = 0;
        for (a, b) in prefix.bytes().zip(line.bytes()) {
            if a != b {
                break;
            }
            common += 1;
        }
        // Two distinct multi-byte characters can share leading bytes before
        // diverging (e.g. box-drawing characters in `tree`-style output), so
        // the byte-level match point isn't guaranteed to land on a char
        // boundary in `prefix` — back off until it does before slicing.
        while common > 0 && !prefix.is_char_boundary(common) {
            common -= 1;
        }
        prefix = &prefix[..common];
        if prefix.is_empty() {
            return "";
        }
    }
    // Trim back to the last `/` so we never split a path segment in half.
    match prefix.rfind('/') {
        Some(pos) => &prefix[..=pos],
        None => "",
    }
}

/// Collapse a shared directory prefix across path-heavy lines. Returns the
/// input unchanged if no prefix meets the savings threshold.
#[must_use]
pub fn collapse_common_prefix(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let candidates = path_bearing_lines(&lines);

    if lines.is_empty() || candidates.len() < MIN_LINES {
        return text.to_string();
    }
    // Line counts are small in practice (this operates on tool output, not
    // multi-gigabyte files), so usize -> f64 precision loss is not a concern.
    #[allow(clippy::cast_precision_loss)]
    let (candidate_count, line_count) = (candidates.len() as f64, lines.len() as f64);
    if candidate_count < MIN_MATCH_RATIO * line_count {
        return text.to_string();
    }

    let prefix = longest_common_dir_prefix(&lines, &candidates);
    if prefix.len() < MIN_PREFIX_LEN {
        return text.to_string();
    }

    let mut out = Vec::with_capacity(lines.len() + 1);
    out.push(format!("[common path prefix: {prefix}]"));
    for (i, line) in lines.iter().enumerate() {
        if candidates.contains(&i) && line.starts_with(prefix) {
            out.push(format!("  {}", &line[prefix.len()..]));
        } else {
            out.push(line.to_string());
        }
    }

    let result = out.join("\n");
    if result.len() < text.len() {
        result
    } else {
        text.to_string()
    }
}

/// Alias IP addresses that share a common dotted-decimal prefix (at least
/// two addresses under the same first-three-octet prefix) to a short
/// `$IP1`-style marker, with a legend line up front. Returns the input
/// unchanged if no prefix is shared by 2+ distinct addresses, or if aliasing
/// wouldn't shrink the text.
#[must_use]
pub fn collapse_ip_prefixes(text: &str) -> String {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for caps in IPV4.captures_iter(text) {
        let prefix = format!("{}.{}.{}.", &caps[1], &caps[2], &caps[3]);
        groups.entry(prefix).or_default().push(caps[0].to_string());
    }

    let mut aliases: Vec<(String, String)> = Vec::new();
    for (prefix, ips) in &groups {
        let mut unique = ips.clone();
        unique.sort();
        unique.dedup();
        if unique.len() >= 2 {
            let alias = format!("$IP{}", aliases.len() + 1);
            aliases.push((prefix.clone(), alias));
        }
    }

    if aliases.is_empty() {
        return text.to_string();
    }

    let mut body = text.to_string();
    for (prefix, alias) in &aliases {
        body = body.replace(prefix.as_str(), &format!("{alias}."));
    }

    let mut lines: Vec<String> = aliases
        .iter()
        .map(|(prefix, alias)| format!("[{alias} = {prefix}]"))
        .collect();
    lines.push(body);
    let assembled = lines.join("\n");

    if assembled.len() < text.len() {
        assembled
    } else {
        text.to_string()
    }
}

/// Collapse a comma-separated run of 3+ `SCREAMING_SNAKE_CASE` tokens (an
/// enum-style value list) into a bracketed, space-free form, e.g.
/// `FEATURE, BUGFIX, HOTFIX` -> `[FEATURE,BUGFIX,HOTFIX]`. Returns the input
/// unchanged if no such run is present or squeezing wouldn't shrink it.
#[must_use]
pub fn collapse_enum_lists(text: &str) -> String {
    let result = ENUM_LIST.replace_all(text, |caps: &regex::Captures<'_>| {
        let squeezed: Vec<&str> = caps[0].split(',').map(str::trim).collect();
        format!("[{}]", squeezed.join(","))
    });

    if result.len() < text.len() {
        result.into_owned()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_shared_prefix() {
        let text = "/Users/tstapler/dotfiles/src/a.rs:1:foo\n\
                     /Users/tstapler/dotfiles/src/b.rs:2:bar\n\
                     /Users/tstapler/dotfiles/src/c.rs:3:baz\n\
                     /Users/tstapler/dotfiles/src/d.rs:4:qux\n\
                     /Users/tstapler/dotfiles/src/e.rs:5:quux";
        let result = collapse_common_prefix(text);
        assert!(result.contains("[common path prefix: /Users/tstapler/dotfiles/src/]"));
        assert!(result.contains("a.rs:1:foo"));
        assert!(!result.contains("/Users/tstapler/dotfiles/src/a.rs"));
    }

    #[test]
    fn no_collapse_below_threshold() {
        let text = "a\nb\nc";
        assert_eq!(collapse_common_prefix(text), text);
    }

    #[test]
    fn no_collapse_when_paths_diverge() {
        let text = "/a/b/x.rs:1\n/c/d/y.rs:2\n/e/f/z.rs:3\n/g/h/w.rs:4\n/i/j/v.rs:5";
        assert_eq!(collapse_common_prefix(text), text);
    }

    #[test]
    fn does_not_panic_on_multibyte_divergence() {
        // Box-drawing characters share leading UTF-8 bytes before diverging
        // mid-character — the byte-level common-prefix scan must not stop at
        // a non-char-boundary offset when slicing.
        let text = "/repo/├── a.rs:1\n\
                     /repo/└── b.rs:2\n\
                     /repo/├── c.rs:3\n\
                     /repo/├── d.rs:4\n\
                     /repo/├── e.rs:5";
        // Must not panic; the exact collapsing behavior isn't the point here.
        let _ = collapse_common_prefix(text);
    }

    #[test]
    fn collapses_shared_ip_prefix() {
        let text = "connecting to 192.168.100.5\n\
                     connecting to 192.168.100.6\n\
                     connecting to 192.168.100.7\n\
                     connecting to 192.168.100.8\n\
                     connecting to 192.168.100.9";
        let result = collapse_ip_prefixes(text);
        assert!(result.contains("[$IP1 = 192.168.100.]"));
        assert!(result.contains("$IP1.5"));
        assert!(result.contains("$IP1.6"));
        assert!(!result.contains("192.168.100.5"));
    }

    #[test]
    fn no_ip_alias_for_single_address() {
        let text = "connecting to 10.0.1.5 only";
        assert_eq!(collapse_ip_prefixes(text), text);
    }

    #[test]
    fn no_ip_alias_when_prefixes_diverge() {
        let text = "10.0.1.5 then 10.0.2.6 then 10.0.3.7";
        assert_eq!(collapse_ip_prefixes(text), text);
    }

    #[test]
    fn collapses_enum_list() {
        let text = "type: FEATURE, BUGFIX, HOTFIX, CHORE";
        let result = collapse_enum_lists(text);
        assert_eq!(result, "type: [FEATURE,BUGFIX,HOTFIX,CHORE]");
    }

    #[test]
    fn no_enum_collapse_below_three_tokens() {
        let text = "type: FEATURE, BUGFIX";
        assert_eq!(collapse_enum_lists(text), text);
    }

    #[test]
    fn no_enum_collapse_for_lowercase_words() {
        let text = "type: feature, bugfix, hotfix";
        assert_eq!(collapse_enum_lists(text), text);
    }
}
