use std::path::Path;

use anyhow::Result;

use crate::claude_code_session::boundary::{
    extract_compaction_metrics, is_compacted, CompactionMetrics,
};
use crate::claude_code_session::native_compaction::{
    extract_native_compaction_events, is_native_compacted, NativeCompactionEvent,
};
use crate::claude_code_session::transcript::{
    build_turns, chain_coverage, parse_session_file, ChainCoverage,
};
use crate::cost_metrics::estimator::TiktokenEstimator;
use crate::cost_metrics::pricing::PricingTable;

/// Estimated cost of running the conversation with no compaction at all,
/// i.e. resending the full growing transcript on every turn.
///
/// This is a naive counterfactual: it ignores prompt caching, so real-world
/// no-compaction costs would typically be lower than this estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct NoCompactionEstimate {
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub pricing_model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionComparison {
    pub no_compaction: NoCompactionEstimate,
    /// Whether the transcript carries any compaction boundary/summary
    /// marker at all. `compaction_metrics` can be empty even when this is
    /// `true` — a compaction run that summarized zero turns (e.g. all turns
    /// fit within `preserve_last_n_turns`) still stamps a boundary marker
    /// but has no per-summary metrics to report.
    pub is_compacted: bool,
    /// Whether Claude Code's own native auto-compaction (a `compact_boundary`
    /// row, independent of any consolette marker) ever ran against this
    /// transcript. Orthogonal to `is_compacted` — a transcript can carry
    /// either, both, or neither kind of marker.
    pub is_native_compacted: bool,
    /// Every native compaction event found in the transcript, in row order.
    /// Empty when `is_native_compacted` is `false`, but can also be empty
    /// even when `is_native_compacted` is `true` (a `compact_boundary` row
    /// with no parseable `compactMetadata` — see
    /// [`crate::claude_code_session::native_compaction`]'s doc comment).
    pub native_events: Vec<NativeCompactionEvent>,
    pub compaction_metrics: Vec<CompactionMetrics>,
    /// How much of the transcript's message history `no_compaction`
    /// actually accounts for. `build_turns` only reconstructs the active
    /// chain ending at the file's last row, so a transcript with
    /// disconnected conversation roots (repeated `--resume`/`--clear`
    /// cycles) yields coverage well below `1.0` — `no_compaction` then only
    /// reflects the most recent thread, not the whole file.
    pub chain_coverage: ChainCoverage,
}

/// Compares the estimated cost of never compacting `session_path` against
/// any real compaction metrics already stamped into it.
///
/// Both sides of the comparison are estimates from `TiktokenEstimator` +
/// `PricingTable`, not live-measured costs from the `claude` CLI.
///
/// # Errors
///
/// Returns an error if the transcript can't be parsed, its turns can't be
/// reconstructed, or token estimation fails for any turn.
#[allow(clippy::cast_precision_loss)]
pub async fn compare_compaction_cost(
    session_path: &Path,
    pricing_model: &str,
) -> Result<CompactionComparison> {
    let rows = parse_session_file(session_path)?;
    let turns = build_turns(&rows)?;
    let coverage = chain_coverage(&rows, &turns);

    let estimator = TiktokenEstimator::new();
    let mut cumulative_tokens = 0u64;
    let mut total_tokens = 0u64;
    for turn in &turns {
        let turn_tokens = super::estimate_turn_tokens(&estimator, pricing_model, turn).await?;
        cumulative_tokens += turn_tokens;
        total_tokens += cumulative_tokens;
    }

    let estimated_cost_usd = PricingTable::load_default()
        .price_for(pricing_model)
        .map(|price| total_tokens as f64 * price.input_usd_per_token);

    let compacted = is_compacted(&rows);
    let compaction_metrics = if compacted {
        extract_compaction_metrics(&rows)
    } else {
        Vec::new()
    };

    // Computed unconditionally: independent of consolette's own
    // `compacted` flag, since a transcript can carry either marker without
    // the other.
    let native_compacted = is_native_compacted(&rows);
    let native_events = extract_native_compaction_events(&rows);

    Ok(CompactionComparison {
        no_compaction: NoCompactionEstimate {
            total_tokens,
            estimated_cost_usd,
            pricing_model: pricing_model.to_string(),
        },
        is_compacted: compacted,
        is_native_compacted: native_compacted,
        native_events,
        compaction_metrics,
        chain_coverage: coverage,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn user_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":{parent_json},"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":{parent_json},"type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"{text}"}}}}"#
        )
    }

    fn write_fixture(lines: &[String]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        file
    }

    #[tokio::test]
    async fn compare_compaction_cost_should_return_empty_metrics_for_uncompacted_transcript() {
        let lines = vec![
            user_row("u1", None, "hello"),
            assistant_row("a1", Some("u1"), "hi there"),
        ];
        let file = write_fixture(&lines);

        let comparison = compare_compaction_cost(file.path(), "claude-sonnet-5")
            .await
            .unwrap();

        assert!(!comparison.is_compacted);
        assert!(comparison.compaction_metrics.is_empty());
        assert!(comparison.no_compaction.total_tokens > 0);
    }

    #[tokio::test]
    async fn compare_compaction_cost_should_report_compacted_with_no_metrics_when_boundary_has_no_summary(
    ) {
        let boundary_marker = r#"{"uuid":"b1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null,"consoletteCompact":{"boundary":true,"prunedCount":0,"sourceSessionId":"src"}}"#
            .to_string();
        let file = write_fixture(&[boundary_marker]);

        let comparison = compare_compaction_cost(file.path(), "claude-sonnet-5")
            .await
            .unwrap();

        assert!(comparison.is_compacted);
        assert!(comparison.compaction_metrics.is_empty());
    }

    #[tokio::test]
    async fn compare_compaction_cost_should_extract_metrics_from_compacted_transcript() {
        let metrics = CompactionMetrics {
            tokens_before: 500,
            tokens_after: 50,
            tokens_saved: 450,
            estimated_cost_usd: Some(0.001_23),
            real_cost_usd: None,
        };
        let user_marker = r#"{"uuid":"u1","parentUuid":null,"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"[consolette: compacted turns summary]"},"consoletteCompact":{"summary":true,"coversTurnUuids":["orig-u1"]}}"#
            .to_string();
        let assistant_marker = format!(
            r#"{{"uuid":"a1","parentUuid":"u1","type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"summary text"}},"consoletteCompact":{{"summary":true,"coversTurnUuids":["orig-u1"],"metrics":{}}}}}"#,
            serde_json::to_string(&metrics).unwrap()
        );
        let file = write_fixture(&[user_marker, assistant_marker]);

        let comparison = compare_compaction_cost(file.path(), "claude-sonnet-5")
            .await
            .unwrap();

        assert_eq!(comparison.compaction_metrics, vec![metrics]);
    }

    /// Task 1.2.2b: a transcript carrying both a native `compact_boundary`
    /// row and a consolette `consoletteCompact` marker reports both flags as
    /// `true` — the two are orthogonal, not mutually exclusive.
    #[tokio::test]
    async fn compare_compaction_cost_should_report_both_native_and_consolette_compaction() {
        let native_boundary = r#"{"uuid":"n1","parentUuid":null,"type":"system","subtype":"compact_boundary","timestamp":"2024-01-01T00:00:00Z","message":null,"compactMetadata":{"trigger":"auto","preTokens":9000,"postTokens":1200}}"#.to_string();
        let consolette_user_marker = r#"{"uuid":"u1","parentUuid":"n1","type":"user","timestamp":"2024-01-01T00:00:01Z","message":{"role":"user","content":"[consolette: compacted turns summary]"},"consoletteCompact":{"summary":true,"coversTurnUuids":["orig-u1"]}}"#.to_string();
        let consolette_assistant_marker = r#"{"uuid":"a1","parentUuid":"u1","type":"assistant","timestamp":"2024-01-01T00:00:02Z","message":{"role":"assistant","content":"summary text"},"consoletteCompact":{"summary":true,"coversTurnUuids":["orig-u1"]}}"#.to_string();
        let file = write_fixture(&[
            native_boundary,
            consolette_user_marker,
            consolette_assistant_marker,
        ]);

        let comparison = compare_compaction_cost(file.path(), "claude-sonnet-5")
            .await
            .unwrap();

        assert!(comparison.is_compacted);
        assert!(comparison.is_native_compacted);
        assert_eq!(comparison.native_events.len(), 1);
        assert_eq!(comparison.native_events[0].tokens_saved(), Some(7800));
    }

    #[tokio::test]
    async fn compare_compaction_cost_should_report_partial_chain_coverage_for_multi_root_transcript(
    ) {
        // Two disconnected conversations in one file, as produced by a
        // `--resume`/`--clear` cycle — build_turns only reconstructs the
        // active chain ending at the last row (a2/u2), so `no_compaction`
        // should not silently claim to cover the earlier u1/a1 exchange.
        let lines = vec![
            user_row("u1", None, "first conversation"),
            assistant_row("a1", Some("u1"), "first reply"),
            user_row("u2", None, "second conversation"),
            assistant_row("a2", Some("u2"), "second reply"),
        ];
        let file = write_fixture(&lines);

        let comparison = compare_compaction_cost(file.path(), "claude-sonnet-5")
            .await
            .unwrap();

        assert_eq!(comparison.chain_coverage.total_messages, 4);
        assert_eq!(comparison.chain_coverage.chain_messages, 2);
        assert!((comparison.chain_coverage.ratio() - 0.5).abs() < f64::EPSILON);
    }
}
