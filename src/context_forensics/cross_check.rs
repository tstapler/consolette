//! Transcript-vs-proxy usage reconciliation (plan.md Story 5.2.1).
//!
//! "Transcript is primary" is enforced at write time, not query time
//! (`research/pitfalls.md` §4): this module never creates a second,
//! competing [`ApiCallRow`](crate::context_forensics::store::ApiCallRow)
//! for a call already recorded from the transcript — it only records the
//! *comparison* between the transcript's reading and an optional
//! proxy-captured one as a
//! [`ProxyCrossCheckRow`](crate::context_forensics::store::ProxyCrossCheckRow).
//! A session with no proxy-side data at all degrades to
//! [`CrossCheckStatus::TranscriptOnly`] for every call — never an error,
//! never a missing row.
//!
//! Nothing in this codebase yet captures per-call proxy usage keyed by
//! message id, so this reconciliation path has no caller yet outside its
//! own unit tests — a later epic wires it into the live proxy-capture path
//! (mirrors `mod.rs`'s [`crate::context_forensics::usage::extract_call_usage`]
//! precedent).

use anyhow::Result;

use crate::context_forensics::store::{
    ContextForensicsStore, CrossCheckStatus, ProxyCrossCheckRow,
};
use crate::providers::AnthropicUsage;

/// Per-field tolerance (summed across all four usage fields) within which
/// two readings of the same call are treated as corroborating rather than
/// diverging — accounts for the small provider-side rounding/timing noise
/// `research/pitfalls.md` doesn't quantify. A starting guess, not a
/// verified constant; revisit once real corroborated/diverged data exists.
#[allow(dead_code)]
const TOLERANCE_TOKENS: i64 = 5;

#[allow(dead_code)]
fn signed_variance(transcript: u64, proxy: u64) -> i64 {
    let transcript = i64::try_from(transcript).unwrap_or(i64::MAX);
    let proxy = i64::try_from(proxy).unwrap_or(i64::MAX);
    transcript - proxy
}

/// Total signed variance (transcript minus proxy, summed across all four
/// usage fields) between two readings of the same call.
#[must_use]
#[allow(dead_code)]
pub(crate) fn usage_variance(transcript: AnthropicUsage, proxy: AnthropicUsage) -> i64 {
    signed_variance(transcript.input_tokens, proxy.input_tokens)
        + signed_variance(transcript.output_tokens, proxy.output_tokens)
        + signed_variance(
            transcript.cache_creation_input_tokens,
            proxy.cache_creation_input_tokens,
        )
        + signed_variance(
            transcript.cache_read_input_tokens,
            proxy.cache_read_input_tokens,
        )
}

/// Compares a transcript-derived reading against an optional proxy-captured
/// one for the same call. Returns the status to record plus the variance
/// (`None` when there's no proxy-side reading to compare against).
#[must_use]
#[allow(dead_code)]
pub(crate) fn reconcile(
    transcript: AnthropicUsage,
    proxy: Option<AnthropicUsage>,
) -> (CrossCheckStatus, Option<i64>) {
    let Some(proxy) = proxy else {
        return (CrossCheckStatus::TranscriptOnly, None);
    };
    let variance = usage_variance(transcript, proxy);
    if variance.abs() <= TOLERANCE_TOKENS {
        (CrossCheckStatus::Corroborated, Some(variance))
    } else {
        (CrossCheckStatus::Diverged, Some(variance))
    }
}

/// A session's aggregate cross-check status for dashboard display (Story
/// 5.2.2): [`CrossCheckStatus::Diverged`] if any call diverged, else
/// [`CrossCheckStatus::Corroborated`] if any call corroborated, else
/// [`CrossCheckStatus::TranscriptOnly`] — the same worst-first precedence a
/// reader would want ("tell me if anything's wrong first").
#[must_use]
pub(crate) fn worst_status(rows: &[ProxyCrossCheckRow]) -> CrossCheckStatus {
    if rows
        .iter()
        .any(|row| row.status == CrossCheckStatus::Diverged)
    {
        CrossCheckStatus::Diverged
    } else if rows
        .iter()
        .any(|row| row.status == CrossCheckStatus::Corroborated)
    {
        CrossCheckStatus::Corroborated
    } else {
        CrossCheckStatus::TranscriptOnly
    }
}

/// Reconciles one call and persists the verdict as a `proxy_cross_check`
/// row, replacing any prior reconciliation for the same call (idempotent
/// re-run — e.g. from a rescan once a later proxy-captured reading
/// arrives).
///
/// # Errors
///
/// Returns an error if the underlying store write fails.
#[allow(dead_code)]
pub(crate) fn record_reconciliation(
    store: &ContextForensicsStore,
    session_id: &str,
    call_id: &str,
    proxy_request_id: Option<&str>,
    transcript: AnthropicUsage,
    proxy: Option<AnthropicUsage>,
) -> Result<CrossCheckStatus> {
    let (status, variance_tokens) = reconcile(transcript, proxy);
    store.upsert_proxy_cross_check(&ProxyCrossCheckRow {
        session_id: session_id.to_string(),
        call_id: call_id.to_string(),
        proxy_request_id: proxy_request_id.map(str::to_string),
        status,
        variance_tokens,
    })?;
    Ok(status)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::context_forensics::store::{SessionRow, Source, UsageProvenance};
    use tempfile::TempDir;

    fn usage(input: u64, output: u64) -> AnthropicUsage {
        AnthropicUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    #[test]
    fn reconcile_should_return_transcript_only_when_no_proxy_reading() {
        let (status, variance) = reconcile(usage(1200, 20), None);
        assert_eq!(status, CrossCheckStatus::TranscriptOnly);
        assert_eq!(variance, None);
    }

    #[test]
    fn reconcile_should_return_corroborated_when_readings_agree() {
        let (status, variance) = reconcile(usage(1200, 20), Some(usage(1200, 20)));
        assert_eq!(status, CrossCheckStatus::Corroborated);
        assert_eq!(variance, Some(0));
    }

    #[test]
    fn reconcile_should_return_corroborated_when_within_tolerance() {
        let (status, _) = reconcile(usage(1200, 20), Some(usage(1203, 20)));
        assert_eq!(status, CrossCheckStatus::Corroborated);
    }

    #[test]
    fn reconcile_should_return_diverged_when_readings_disagree_beyond_tolerance() {
        let (status, variance) = reconcile(usage(1200, 20), Some(usage(900, 20)));
        assert_eq!(status, CrossCheckStatus::Diverged);
        assert_eq!(variance, Some(300));
    }

    fn open_store(dir: &TempDir) -> ContextForensicsStore {
        let store = ContextForensicsStore::open(&dir.path().join("store.sqlite")).unwrap();
        store
            .upsert_session(&SessionRow {
                id: "s1".to_string(),
                source: Source::ClaudeCode,
                path: "/tmp/s1.jsonl".to_string(),
                project: None,
                started_at: Some("2026-01-01T00:00:00Z".to_string()),
                last_ingested_at: "2026-01-01T00:05:00Z".to_string(),
                chain_coverage_ratio: Some(1.0),
                parse_failure_count: 0,
            })
            .unwrap();
        store
            .upsert_api_call(&crate::context_forensics::store::ApiCallRow {
                id: "s1:msg_abc".to_string(),
                session_id: "s1".to_string(),
                turn_id: None,
                row_uuid: "msg_abc".to_string(),
                call_index: 0,
                model: None,
                input_tokens: 1200,
                output_tokens: 20,
                cache_creation_input_tokens: Some(0),
                cache_read_input_tokens: Some(0),
                tool_io_tokens: 0,
                conversation_tokens: 1200,
                system_tokens: 0,
                usage_provenance: UsageProvenance::TranscriptExact,
                message_json: None,
            })
            .unwrap();
        store
    }

    #[test]
    fn record_reconciliation_should_persist_transcript_only_when_no_proxy_data_exists() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);

        let status =
            record_reconciliation(&store, "s1", "s1:msg_abc", None, usage(1200, 20), None).unwrap();

        assert_eq!(status, CrossCheckStatus::TranscriptOnly);
        let rows = store.proxy_cross_check_for_session("s1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, CrossCheckStatus::TranscriptOnly);
    }

    #[test]
    fn record_reconciliation_should_replace_prior_verdict_when_run_twice() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);

        record_reconciliation(&store, "s1", "s1:msg_abc", None, usage(1200, 20), None).unwrap();
        record_reconciliation(
            &store,
            "s1",
            "s1:msg_abc",
            Some("req_1"),
            usage(1200, 20),
            Some(usage(1200, 20)),
        )
        .unwrap();

        let rows = store.proxy_cross_check_for_session("s1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, CrossCheckStatus::Corroborated);
    }

    fn cross_check_row(status: CrossCheckStatus) -> ProxyCrossCheckRow {
        ProxyCrossCheckRow {
            session_id: "s1".to_string(),
            call_id: "s1:msg_abc".to_string(),
            proxy_request_id: None,
            status,
            variance_tokens: None,
        }
    }

    #[test]
    fn worst_status_should_return_transcript_only_when_rows_empty() {
        assert_eq!(worst_status(&[]), CrossCheckStatus::TranscriptOnly);
    }

    #[test]
    fn worst_status_should_return_corroborated_when_one_corroborated_row() {
        let rows = [cross_check_row(CrossCheckStatus::Corroborated)];
        assert_eq!(worst_status(&rows), CrossCheckStatus::Corroborated);
    }

    #[test]
    fn worst_status_should_return_diverged_when_any_row_diverged() {
        let rows = [
            cross_check_row(CrossCheckStatus::Corroborated),
            cross_check_row(CrossCheckStatus::Diverged),
            cross_check_row(CrossCheckStatus::Corroborated),
        ];
        assert_eq!(worst_status(&rows), CrossCheckStatus::Diverged);
    }

    #[test]
    fn cross_check_status_for_session_should_return_diverged_when_any_call_diverged() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        record_reconciliation(
            &store,
            "s1",
            "s1:msg_abc",
            Some("req_1"),
            usage(1200, 20),
            Some(usage(900, 20)),
        )
        .unwrap();

        assert_eq!(
            store.cross_check_status_for_session("s1").unwrap(),
            CrossCheckStatus::Diverged
        );
    }
}
