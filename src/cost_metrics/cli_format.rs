//! Rendering for `consolette cost-report` (Epic 3.1, Tasks 3.1.1d/e).
//!
//! Both formatters read the same [`CostReport`] value fetched by
//! [`crate::cost_metrics::client::fetch_cost_report`] — there is no
//! separate JSON-construction path (ux.md AC3), so table and JSON output
//! cannot drift from each other or from the HTTP surface.

use std::fmt::Write as _;

use crate::cost_metrics::report::CostReport;
use crate::cost_metrics::types::{PricingSource, TokenSource};

/// `serde_json::to_string_pretty(&report)`, verbatim — the exact bytes
/// `--json` prints (Task 3.1.1e).
///
/// # Panics
///
/// Panics if `report` somehow fails to serialize; `CostReport` derives
/// `Serialize` over plain, always-serializable field types, so this should
/// not happen in practice.
#[must_use]
#[allow(clippy::expect_used)]
pub fn format_cost_report_json(report: &CostReport) -> String {
    serde_json::to_string_pretty(report).expect("CostReport should always serialize")
}

/// Hand-formatted, aligned text table (Task 3.1.1d) — the sample output
/// shape from `design/ux.md`'s Surface 1.
#[must_use]
pub fn format_cost_report_table(report: &CostReport) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "session_key: {}", report.session_key);
    let _ = writeln!(
        out,
        "pricing_source: {}",
        pricing_source_text(report.pricing_source)
    );
    out.push('\n');

    write_summary_lines(&mut out, report);
    out.push('\n');

    out.push_str("by_tier:\n");
    for tier in &report.by_tier {
        let counterfactual = tier
            .counterfactual_tokens
            .map_or_else(|| "unavailable".to_string(), format_count);
        let compacted = tier
            .compacted_tokens
            .map_or_else(|| "unavailable".to_string(), format_count);
        let actual = tier
            .actual_tokens
            .map_or_else(|| "unavailable".to_string(), format_count);
        let saved = match (tier.tokens_saved, tier.counterfactual_tokens) {
            (Some(saved), Some(counterfactual)) => {
                format!(
                    "{} ({})",
                    format_count(saved),
                    format_percent(saved, counterfactual)
                )
            }
            _ => "unavailable".to_string(),
        };
        let _ = writeln!(
            out,
            "  {:?}  counterfactual={counterfactual}  compacted={compacted}  saved={saved}  actual={actual}",
            tier.tier
        );
    }
    out.push('\n');

    let _ = writeln!(
        out,
        "pending: {}   abandoned: {}",
        report.pending_count, report.abandoned_count
    );

    out
}

/// The six `actual_*`/`counterfactual_*`/`tokens_saved`/`estimated_cost_saved_usd`
/// lines, split out of [`format_cost_report_table`] to keep that function
/// under clippy's `too_many_lines` limit — this is a straight-line
/// extraction, no behavior change.
fn write_summary_lines(out: &mut String, report: &CostReport) {
    let _ = writeln!(
        out,
        "actual_tokens:            {}",
        match (report.actual_tokens, report.actual_source) {
            (Some(tokens), Some(source)) => {
                format!("{}  {}", format_count(tokens), source_suffix(source))
            }
            _ => "unavailable — request did not complete".to_string(),
        }
    );
    let _ = writeln!(
        out,
        "actual_cost_usd:          {}",
        match report.actual_cost_usd {
            Some(cost) => format!(
                "{}  ({} pricing)",
                format_usd(cost),
                pricing_source_text(report.pricing_source)
            ),
            None => "unavailable — request did not complete".to_string(),
        }
    );
    let _ = writeln!(
        out,
        "counterfactual_tokens:    {}",
        match (report.counterfactual_tokens, report.counterfactual_source) {
            (Some(tokens), Some(source)) => {
                format!("{}  {}", format_count(tokens), source_suffix(source))
            }
            _ => "unavailable".to_string(),
        }
    );
    let _ = writeln!(
        out,
        "compacted_tokens:         {}",
        match (report.compacted_tokens, report.counterfactual_source) {
            (Some(tokens), Some(source)) => {
                format!("{}  {}", format_count(tokens), source_suffix(source))
            }
            (Some(tokens), None) => format_count(tokens),
            (None, _) => "unavailable — not yet reconciled".to_string(),
        }
    );
    let _ = writeln!(
        out,
        "tokens_saved:             {}",
        match (report.tokens_saved, report.counterfactual_tokens) {
            (Some(saved), Some(counterfactual)) => {
                format!(
                    "{} ({})",
                    format_count(saved),
                    format_percent(saved, counterfactual)
                )
            }
            _ => "unavailable (not yet reconciled)".to_string(),
        }
    );
    let _ = writeln!(
        out,
        "estimated_cost_saved_usd: {}",
        match report.estimated_cost_saved_usd {
            Some(cost) => format!(
                "{}  ({} pricing)",
                format_usd(cost),
                pricing_source_text(report.pricing_source)
            ),
            None => "unavailable (not yet reconciled)".to_string(),
        }
    );
}

fn pricing_source_text(source: PricingSource) -> &'static str {
    match source {
        PricingSource::Static => "static",
        PricingSource::Live => "live",
    }
}

/// `(exact)` or `(estimated via <EstimatorKind>)` — stated as literal text
/// on every token figure, never conveyed by color alone (ux.md AC1/AC9), so
/// it survives `NO_COLOR=1` or being piped to a file unchanged.
fn source_suffix(source: TokenSource) -> String {
    match source {
        TokenSource::Exact => "(exact)".to_string(),
        TokenSource::Estimated { via } => format!("(estimated via {via:?})"),
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
}

/// Casts token counts to `f64` for a display-only percentage; both sides
/// are session-scoped token counts, far below `f64`'s 52-bit mantissa, so
/// the loss `cast_precision_loss` warns about cannot occur in practice.
#[allow(clippy::cast_precision_loss)]
fn format_percent(saved: u64, counterfactual: u64) -> String {
    if counterfactual == 0 {
        return "0.0%".to_string();
    }
    let pct = (saved as f64 / counterfactual as f64) * 100.0;
    format!("{pct:.1}%")
}

fn format_usd(amount: f64) -> String {
    format!("${amount:.2}")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use crate::cost_metrics::report::TierBreakdown;
    use crate::cost_metrics::types::{EstimatorKind, PricingSource, TokenSource};
    use crate::session_compaction::CompactionTier;

    use super::*;

    fn base_report() -> CostReport {
        CostReport {
            session_key: "s1".to_string(),
            actual_tokens: Some(4000),
            actual_source: Some(TokenSource::Exact),
            actual_cost_usd: Some(0.01),
            counterfactual_tokens: Some(12000),
            counterfactual_source: Some(TokenSource::Estimated {
                via: EstimatorKind::AnthropicCountTokensApi,
            }),
            compacted_tokens: Some(4200),
            tokens_saved: Some(7800),
            estimated_cost_saved_usd: Some(0.02),
            pricing_source: PricingSource::Static,
            pending_count: 0,
            abandoned_count: 0,
            by_tier: vec![TierBreakdown {
                tier: CompactionTier::Full,
                counterfactual_tokens: Some(12000),
                compacted_tokens: Some(4200),
                actual_tokens: Some(4000),
                tokens_saved: Some(7800),
                cost_counterfactual_usd: None,
                cost_actual_usd: None,
            }],
        }
    }

    /// Task 3.1.1f: percentage math.
    #[test]
    fn format_cost_report_table_should_compute_percentage_when_tokens_saved_present() {
        let table = format_cost_report_table(&base_report());
        assert!(
            table.contains("tokens_saved:             7,800 (65.0%)"),
            "got:\n{table}"
        );
    }

    /// Task 3.1.1f: `$` formatting.
    #[test]
    fn format_cost_report_table_should_render_dollar_amount_when_cost_present() {
        let table = format_cost_report_table(&base_report());
        assert!(
            table.contains("estimated_cost_saved_usd: $0.02"),
            "got:\n{table}"
        );
    }

    /// Task 3.1.1f / ux.md AC4: missing price renders literal `unavailable`,
    /// never `$0.00`.
    #[test]
    fn format_cost_report_table_should_render_unavailable_when_estimated_cost_saved_usd_is_none() {
        let mut report = base_report();
        report.estimated_cost_saved_usd = None;

        let table = format_cost_report_table(&report);
        assert!(
            table.contains("estimated_cost_saved_usd: unavailable"),
            "got:\n{table}"
        );
        assert!(!table.contains("$0.00"), "got:\n{table}");
    }

    /// ux.md AC1/AC9: exact/estimated is a text suffix, never color-only.
    #[test]
    fn format_cost_report_table_should_print_estimated_via_suffix_when_source_is_estimated() {
        let table = format_cost_report_table(&base_report());
        assert!(
            table.contains("(estimated via AnthropicCountTokensApi)"),
            "got:\n{table}"
        );
        assert!(table.contains("(exact)"), "got:\n{table}");
    }

    /// ux.md: `--json` is byte-for-byte `serde_json::to_string_pretty` on
    /// the same value the table is rendered from — no separate
    /// JSON-construction path.
    #[test]
    fn format_cost_report_json_should_match_serde_to_string_pretty_when_given_same_report() {
        let report = base_report();
        let expected = serde_json::to_string_pretty(&report).expect("should serialize");
        assert_eq!(format_cost_report_json(&report), expected);
    }

    #[test]
    fn format_cost_report_table_should_render_pending_state_when_never_reconciled() {
        let report = CostReport {
            session_key: "s2".to_string(),
            actual_tokens: None,
            actual_source: None,
            actual_cost_usd: None,
            counterfactual_tokens: Some(9800),
            counterfactual_source: Some(TokenSource::Estimated {
                via: EstimatorKind::TiktokenO200k,
            }),
            compacted_tokens: None,
            tokens_saved: None,
            estimated_cost_saved_usd: None,
            pricing_source: PricingSource::Static,
            pending_count: 1,
            abandoned_count: 0,
            by_tier: vec![],
        };

        let table = format_cost_report_table(&report);
        assert!(
            table.contains("actual_tokens:            unavailable — request did not complete"),
            "got:\n{table}"
        );
        assert!(
            table.contains("tokens_saved:             unavailable (not yet reconciled)"),
            "got:\n{table}"
        );
        assert!(table.contains("pending: 1   abandoned: 0"), "got:\n{table}");
    }
}
