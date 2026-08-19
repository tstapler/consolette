//! Detection and extraction of Claude Code's own *native* auto-compaction
//! events, as distinct from consolette's own `consoletteCompact` markers
//! (see [`crate::claude_code_session::boundary`]).
//!
//! Claude Code stamps a native compaction as a `type: "system"`,
//! `subtype: "compact_boundary"` row carrying a `compactMetadata` object.
//! Every field of that object is treated as optional here: real-world
//! transcripts have been observed with a `compact_boundary` row and no
//! `compactMetadata` at all (an older Claude Code build, or a boundary from
//! a slash-command `/compact` invocation), and any field shape mismatch is
//! skipped-and-logged via `serde_json::from_value(..).ok()` rather than
//! propagated as an error — mirroring
//! [`crate::claude_code_session::boundary`]'s tolerance for the same class
//! of forward/backward-compatibility drift in a format this crate doesn't
//! control.

use crate::claude_code_session::transcript::TranscriptRow;
use serde::Deserialize;

/// One native Claude Code auto-compaction event, parsed from a
/// `compact_boundary` row's `compactMetadata` object. Every field is
/// `Option` because the real-world shape of `compactMetadata` is not
/// controlled by this crate and has been observed to vary or be absent
/// entirely.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NativeCompactionEvent {
    pub trigger: Option<String>,
    #[serde(rename = "preTokens")]
    pub pre_tokens: Option<u64>,
    #[serde(rename = "postTokens")]
    pub post_tokens: Option<u64>,
    #[serde(rename = "durationMs")]
    pub duration_ms: Option<u64>,
    #[serde(rename = "cumulativeDroppedTokens")]
    pub cumulative_dropped_tokens: Option<u64>,
    #[serde(rename = "preservedSegment")]
    pub preserved_segment_tokens: Option<u64>,
    #[serde(rename = "preservedMessages")]
    pub preserved_messages: Option<u64>,
    #[serde(rename = "preCompactDiscoveredTools")]
    pub discovered_tools: Option<Vec<String>>,
}

impl NativeCompactionEvent {
    /// `pre_tokens - post_tokens`, when both are known. `None` when either
    /// side of the subtraction is unavailable — never guessed at.
    #[must_use]
    pub fn tokens_saved(&self) -> Option<i64> {
        match (self.pre_tokens, self.post_tokens) {
            (Some(pre), Some(post)) => Some(
                i64::try_from(pre).unwrap_or(i64::MAX) - i64::try_from(post).unwrap_or(i64::MAX),
            ),
            _ => None,
        }
    }
}

/// `true` when `row` is a native Claude Code compaction boundary marker
/// (`type: "system"`, `subtype: "compact_boundary"`), regardless of whether
/// it carries `compactMetadata`.
fn is_compact_boundary_row(row: &TranscriptRow) -> bool {
    matches!(row, TranscriptRow::System(_))
        && row
            .fields()
            .extra
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            == Some("compact_boundary")
}

/// `true` when `rows` contains at least one native `compact_boundary`
/// marker — i.e. Claude Code itself auto-compacted this transcript at some
/// point, independent of whether consolette also has (see
/// [`crate::claude_code_session::boundary::is_compacted`] for that).
#[must_use]
pub fn is_native_compacted(rows: &[TranscriptRow]) -> bool {
    rows.iter().any(is_compact_boundary_row)
}

/// Extract every [`NativeCompactionEvent`] embedded in `rows`' native
/// `compact_boundary` markers, in row order. A `compact_boundary` row with
/// no `compactMetadata`, or one whose `compactMetadata` doesn't deserialize
/// (unexpected shape), contributes no entry — this is a best-effort
/// extraction, not a strict parse.
#[must_use]
pub fn extract_native_compaction_events(rows: &[TranscriptRow]) -> Vec<NativeCompactionEvent> {
    rows.iter()
        .filter(|row| is_compact_boundary_row(row))
        .filter_map(|row| {
            let metadata = row.fields().extra.get("compactMetadata")?;
            serde_json::from_value(metadata.clone()).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn compact_boundary_row_with_metadata(metadata_json: &str) -> TranscriptRow {
        let line = format!(
            r#"{{"uuid":"b1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null,"compactMetadata":{metadata_json}}}"#
        );
        serde_json::from_str(&line).unwrap()
    }

    fn compact_boundary_row_without_metadata() -> TranscriptRow {
        let line = r#"{"uuid":"b1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null}"#;
        serde_json::from_str(line).unwrap()
    }

    fn user_row(uuid: &str, text: &str) -> TranscriptRow {
        let line = format!(
            r#"{{"uuid":"{uuid}","parentUuid":null,"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        );
        serde_json::from_str(&line).unwrap()
    }

    /// Task 1.1.1a/d: given a `compact_boundary` row with full
    /// `compactMetadata`, `is_native_compacted` is true,
    /// `extract_native_compaction_events` returns the parsed event, and
    /// `.tokens_saved()` reports `preTokens - postTokens`.
    #[test]
    fn native_compaction_should_detect_and_extract_event_when_full_metadata_present() {
        let row = compact_boundary_row_with_metadata(
            r#"{"trigger":"auto","preTokens":9000,"postTokens":1200,"durationMs":842,"cumulativeDroppedTokens":7800}"#,
        );
        let rows = vec![row];

        assert!(is_native_compacted(&rows));

        let events = extract_native_compaction_events(&rows);
        assert_eq!(
            events,
            vec![NativeCompactionEvent {
                trigger: Some("auto".to_string()),
                pre_tokens: Some(9000),
                post_tokens: Some(1200),
                duration_ms: Some(842),
                cumulative_dropped_tokens: Some(7800),
                preserved_segment_tokens: None,
                preserved_messages: None,
                discovered_tools: None,
            }]
        );
        assert_eq!(events[0].tokens_saved(), Some(7800));
    }

    /// Task 1.1.2a: a `compact_boundary` row with no `compactMetadata` at
    /// all is still detected as native-compacted, but contributes no event.
    #[test]
    fn native_compaction_should_report_boundary_with_no_events_when_metadata_missing() {
        let rows = vec![compact_boundary_row_without_metadata()];

        assert!(is_native_compacted(&rows));
        assert_eq!(extract_native_compaction_events(&rows), vec![]);
    }

    /// Task 1.1.2a: a malformed `compactMetadata` (wrong shape) is skipped,
    /// never panics, and doesn't affect extraction of a sibling valid one.
    #[test]
    fn native_compaction_should_skip_malformed_metadata_without_panicking() {
        let malformed = compact_boundary_row_with_metadata(r#""not-an-object""#);
        let valid = compact_boundary_row_with_metadata(
            r#"{"trigger":"manual","preTokens":100,"postTokens":10}"#,
        );
        let rows = vec![malformed, valid];

        let events = extract_native_compaction_events(&rows);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].trigger, Some("manual".to_string()));
    }

    /// Task 1.1.2b: a transcript with zero `compact_boundary` rows is not
    /// native-compacted and yields no events.
    #[test]
    fn native_compaction_should_return_false_and_empty_when_no_boundary_row_present() {
        let rows = vec![user_row("u1", "hello")];

        assert!(!is_native_compacted(&rows));
        assert_eq!(extract_native_compaction_events(&rows), vec![]);
    }

    #[test]
    fn tokens_saved_should_be_none_when_either_side_unknown() {
        let event = NativeCompactionEvent {
            trigger: None,
            pre_tokens: Some(100),
            post_tokens: None,
            duration_ms: None,
            cumulative_dropped_tokens: None,
            preserved_segment_tokens: None,
            preserved_messages: None,
            discovered_tools: None,
        };
        assert_eq!(event.tokens_saved(), None);
    }
}
