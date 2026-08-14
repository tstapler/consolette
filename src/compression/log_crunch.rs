//! `LogCrunch`: occurrence-count folding of near-repeated non-critical log
//! lines, with explicit protection for ERROR/WARN/FATAL/stack-trace lines,
//! plus timestamp relativization to deltas.
//!
//! [`crate::compression::text_compressor`]'s `dedup_consecutive_lines` /
//! `dedup_log_timestamps` only collapse byte-identical *consecutive* lines.
//! This module extends that to lines that recur anywhere in the text (not
//! just adjacently) after normalizing timestamps and numeric IDs, while
//! never touching a line that carries an ERROR/WARN/FATAL/CRITICAL level or
//! looks like a stack-trace frame — those must always survive verbatim.

use chrono::{DateTime, FixedOffset};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

/// Templates recurring at least this many times are folded to a single
/// annotated occurrence.
const FOLD_THRESHOLD: usize = 3;

#[allow(clippy::expect_used)]
static ISO_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?")
        .expect("ISO_TIMESTAMP regex")
});

#[allow(clippy::expect_used)]
static NUMERIC_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("NUMERIC_RUN regex"));

#[allow(clippy::expect_used)]
static LOG_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(ERROR|WARN(?:ING)?|FATAL|CRITICAL)\b").expect("LOG_LEVEL regex")
});

#[allow(clippy::expect_used)]
static STACK_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(at\s+\S|Caused by:|Traceback \(most recent call last\)|File ")"#)
        .expect("STACK_FRAME regex")
});

/// True for lines that must never be folded away or relativized: error/warn/
/// fatal/critical level markers and stack-trace frames.
fn is_protected(line: &str) -> bool {
    LOG_LEVEL.is_match(line) || STACK_FRAME.is_match(line)
}

/// Line template used to group near-repeated (not just byte-identical) log
/// lines: ISO timestamps and standalone digit runs are replaced with
/// placeholders so e.g. `request id=123 took 45ms` and `request id=456 took
/// 12ms` compare equal.
fn line_template(line: &str) -> String {
    let no_ts = ISO_TIMESTAMP.replace_all(line, "{ts}");
    NUMERIC_RUN.replace_all(&no_ts, "{n}").into_owned()
}

/// Fold non-protected log lines that recur [`FOLD_THRESHOLD`]+ times (by
/// [`line_template`], not necessarily consecutively) down to their first
/// occurrence annotated with a total count; later occurrences are dropped.
/// ERROR/WARN/FATAL/CRITICAL lines and stack-trace frames are always kept
/// verbatim and never counted toward another template's fold. Returns the
/// input unchanged if folding wouldn't shrink the text.
#[must_use]
pub fn fold_occurrence_counts(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();

    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in &lines {
        if is_protected(line) {
            continue;
        }
        *counts.entry(line_template(line)).or_insert(0) += 1;
    }

    let mut folded: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());

    for &line in &lines {
        if is_protected(line) {
            out.push(line.to_string());
            continue;
        }

        let template = line_template(line);
        let total = *counts.get(&template).unwrap_or(&0);
        if total < FOLD_THRESHOLD {
            out.push(line.to_string());
            continue;
        }

        if folded.contains(&template) {
            continue; // already represented by its first occurrence
        }
        folded.insert(template);
        out.push(format!("{line} [occurred {total}x]"));
    }

    let result = out.join("\n");
    if result.len() < text.len() {
        result
    } else {
        text.to_string()
    }
}

/// Replace ISO 8601 timestamps after the first one in `text` with a `+Ns`
/// delta from the immediately preceding timestamp, so a run of near-full
/// timestamps compresses to a few characters each. A timestamp that fails to
/// parse (e.g. missing a timezone offset, so it's ambiguous) is left as-is
/// and doesn't reset the delta chain.
#[must_use]
pub fn relativize_timestamps(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut prev: Option<DateTime<FixedOffset>> = None;

    for m in ISO_TIMESTAMP.find_iter(text) {
        result.push_str(&text[last_end..m.start()]);
        let raw = m.as_str();

        match (DateTime::parse_from_rfc3339(raw).ok(), prev) {
            (Some(ts), Some(p)) => {
                let millis = ts.signed_duration_since(p).num_milliseconds();
                #[allow(clippy::cast_precision_loss)]
                let secs = millis as f64 / 1000.0;
                let _ = write!(result, "+{secs:.3}s");
                prev = Some(ts);
            }
            (Some(ts), None) => {
                result.push_str(raw);
                prev = Some(ts);
            }
            (None, _) => result.push_str(raw),
        }
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);

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
    fn folds_non_consecutive_repeated_info_lines() {
        let text = "INFO: heartbeat ok id=1\n\
                     something unrelated happened here\n\
                     INFO: heartbeat ok id=2\n\
                     another unrelated line goes here\n\
                     INFO: heartbeat ok id=3";
        let result = fold_occurrence_counts(text);
        assert!(result.contains("[occurred 3x]"));
        assert!(!result.contains("id=2"));
        assert!(!result.contains("id=3"));
        assert!(result.contains("something unrelated happened here"));
    }

    #[test]
    fn never_folds_error_lines() {
        let text = "ERROR: disk full on /dev/sda1\n\
                     ERROR: disk full on /dev/sda1\n\
                     ERROR: disk full on /dev/sda1\n\
                     ERROR: disk full on /dev/sda1";
        assert_eq!(fold_occurrence_counts(text), text);
    }

    #[test]
    fn never_folds_stack_trace_frames() {
        let text = "Traceback (most recent call last):\n\
                     \tat com.example.Foo.bar(Foo.java:12)\n\
                     \tat com.example.Foo.bar(Foo.java:12)\n\
                     \tat com.example.Foo.bar(Foo.java:12)";
        assert_eq!(fold_occurrence_counts(text), text);
    }

    #[test]
    fn leaves_below_threshold_untouched() {
        let text = "INFO: tick id=1\nINFO: tick id=2";
        assert_eq!(fold_occurrence_counts(text), text);
    }

    #[test]
    fn relativizes_repeated_timestamps_to_deltas() {
        let text = "2026-08-14T10:00:00.000Z request started processing the payload\n\
                     2026-08-14T10:00:01.500Z request still processing the payload\n\
                     2026-08-14T10:00:03.250Z request finished processing the payload";
        let result = relativize_timestamps(text);
        assert!(result.starts_with("2026-08-14T10:00:00.000Z"));
        assert!(result.contains("+1.500s"));
        assert!(result.contains("+1.750s"));
        assert!(!result.contains("2026-08-14T10:00:01"));
        assert!(!result.contains("2026-08-14T10:00:03"));
    }

    #[test]
    fn single_timestamp_is_left_absolute() {
        let text = "2026-08-14T10:00:00.000Z the only timestamped line in this block";
        assert_eq!(relativize_timestamps(text), text);
    }
}
