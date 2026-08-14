//! `QuantumLock`/`CachePrefixManager`: stabilize dynamic fragments in system
//! prompt text so [`crate::system_prompt::cache_aligner`] can add
//! `cache_control` to it instead of bailing out.
//!
//! `CacheAligner` currently just refuses to cache any system block containing
//! a UUID or ISO 8601 timestamp, since that content differs every request
//! and would never hit. This stage instead rewrites such fragments to stable
//! placeholders and moves the real values into a separate trailer block
//! appended after the (now-stable) main block — so the cacheable prefix
//! stays byte-identical across requests and Anthropic's prompt-cache-read
//! discount applies to it, while only the small trailer block (which never
//! gets `cache_control`) changes per request.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

// Compile-time constant patterns; a build-time failure here would be a
// programmer error, not a runtime one.
#[allow(clippy::unwrap_used)]
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});

#[allow(clippy::unwrap_used)]
static RE_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?").unwrap()
});

/// Bearer/session-token-shaped fragments: a labeled key followed by a long
/// opaque alphanumeric run, e.g. `token=abc123...`, `Bearer sk-abc...`.
#[allow(clippy::unwrap_used)]
static RE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:bearer|token|session|api[_-]?key)[=:\s]+[A-Za-z0-9_\-\.]{16,}").unwrap()
});

/// One dynamic fragment found in the source text, in order of first
/// appearance.
struct Match {
    start: usize,
    end: usize,
    value: String,
}

fn find_dynamic_fragments(text: &str) -> Vec<Match> {
    let mut matches: Vec<Match> = Vec::new();
    for re in [&*RE_UUID, &*RE_TIMESTAMP, &*RE_TOKEN] {
        for m in re.find_iter(text) {
            matches.push(Match {
                start: m.start(),
                end: m.end(),
                value: m.as_str().to_string(),
            });
        }
    }
    matches.sort_by_key(|m| m.start);

    // Drop matches that overlap an earlier (already-sorted, earlier-starting)
    // match — e.g. a timestamp match nested inside a longer token match.
    let mut out: Vec<Match> = Vec::with_capacity(matches.len());
    let mut last_end = 0;
    for m in matches {
        if m.start < last_end {
            continue;
        }
        last_end = m.end;
        out.push(m);
    }
    out
}

/// Replace dynamic fragments (UUIDs, ISO 8601 timestamps, bearer/session
/// tokens) in `text` with stable `{{QL_n}}` placeholders, and return the
/// real values as a delimited trailer block to append separately. Identical
/// values reuse the same placeholder. Returns `None` for the trailer if no
/// dynamic fragments were found, in which case the text is returned
/// unmodified.
#[must_use]
pub fn stabilize_dynamic_fragments(text: &str) -> (String, Option<String>) {
    let fragments = find_dynamic_fragments(text);
    if fragments.is_empty() {
        return (text.to_string(), None);
    }

    let mut placeholder_for: HashMap<String, usize> = HashMap::new();
    let mut ordered_values: Vec<String> = Vec::new();
    let mut stable = String::with_capacity(text.len());
    let mut last_end = 0;

    for frag in &fragments {
        stable.push_str(&text[last_end..frag.start]);
        let idx = *placeholder_for
            .entry(frag.value.clone())
            .or_insert_with(|| {
                ordered_values.push(frag.value.clone());
                ordered_values.len()
            });
        let _ = write!(stable, "{{{{QL_{idx}}}}}");
        last_end = frag.end;
    }
    stable.push_str(&text[last_end..]);

    let mut trailer = String::from("Dynamic values referenced above by placeholder:\n");
    for (i, value) in ordered_values.iter().enumerate() {
        let _ = writeln!(trailer, "QL_{}={value}", i + 1);
    }

    (stable, Some(trailer))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;

    #[test]
    fn replaces_uuid_with_placeholder_and_returns_trailer() {
        let text = "session id=550e8400-e29b-41d4-a716-446655440000 ready";
        let (stable, trailer) = stabilize_dynamic_fragments(text);
        assert_eq!(stable, "session id={{QL_1}} ready");
        let trailer = trailer.expect("expected a trailer");
        assert!(trailer.contains("QL_1=550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn replaces_timestamp_with_placeholder() {
        let text = "started at 2026-08-14T10:00:00Z running";
        let (stable, trailer) = stabilize_dynamic_fragments(text);
        assert_eq!(stable, "started at {{QL_1}} running");
        assert!(trailer.unwrap().contains("QL_1=2026-08-14T10:00:00Z"));
    }

    #[test]
    fn reuses_placeholder_for_repeated_value() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let text = format!("id={id} again id={id}");
        let (stable, trailer) = stabilize_dynamic_fragments(&text);
        assert_eq!(stable, "id={{QL_1}} again id={{QL_1}}");
        assert_eq!(trailer.unwrap().matches("QL_1=").count(), 1);
    }

    #[test]
    fn no_dynamic_content_returns_none_trailer() {
        let text = "You are a helpful coding assistant.";
        let (stable, trailer) = stabilize_dynamic_fragments(text);
        assert_eq!(stable, text);
        assert!(trailer.is_none());
    }

    #[test]
    fn multiple_distinct_values_get_distinct_placeholders() {
        let text = "a=2026-08-14T10:00:00Z b=2026-08-14T11:00:00Z";
        let (stable, trailer) = stabilize_dynamic_fragments(text);
        assert_eq!(stable, "a={{QL_1}} b={{QL_2}}");
        let trailer = trailer.unwrap();
        assert!(trailer.contains("QL_1=2026-08-14T10:00:00Z"));
        assert!(trailer.contains("QL_2=2026-08-14T11:00:00Z"));
    }
}
