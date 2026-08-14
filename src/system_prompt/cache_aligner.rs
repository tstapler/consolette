//! `CacheAligner`: stability checks for KV cache control markers.

use std::sync::LazyLock;

use regex::Regex;

// These patterns are compile-time constants validated by the tests below;
// a build-time failure here would be a programmer error, not a runtime one.
#[allow(clippy::unwrap_used)]
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});

#[allow(clippy::unwrap_used)]
static RE_TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}").unwrap());

/// Returns `true` if `cache_control` should be added to a system block with this text.
///
/// Returns `false` when:
/// - text is shorter than 100 chars (below Anthropic minimum cache threshold)
/// - text contains UUIDs (volatile, cache would never hit)
/// - text contains ISO 8601 timestamps (volatile, cache would never hit)
#[must_use]
pub fn should_add_cache_control(text: &str) -> bool {
    if text.len() < 100 {
        return false;
    }
    if RE_UUID.is_match(text) {
        return false;
    }
    if RE_TIMESTAMP.is_match(text) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_returns_false() {
        assert!(!should_add_cache_control("Be helpful."));
    }

    #[test]
    fn uuid_returns_false() {
        let text = "a".repeat(200) + " id=550e8400-e29b-41d4-a716-446655440000";
        assert!(!should_add_cache_control(&text));
    }

    #[test]
    fn timestamp_returns_false() {
        let text = "a".repeat(200) + " ts=2026-06-18T12:00:00";
        assert!(!should_add_cache_control(&text));
    }

    #[test]
    fn stable_long_text_returns_true() {
        let text = "You are a helpful coding assistant. ".repeat(10);
        assert!(should_add_cache_control(&text));
    }
}
