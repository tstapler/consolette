//! Exact `usage.*` extraction from a Claude Code transcript row
//! (context-analyzer plan.md Story 1.1.3).

use crate::claude_code_session::transcript::TranscriptRow;
use crate::providers::AnthropicUsage;

/// Read a transcript row's `message.usage` object and parse it into the
/// same [`AnthropicUsage`] shape [`crate::providers::extract_usage`]
/// produces from a live Anthropic response — exact API-reported counts,
/// not [`crate::cost_metrics::estimator::TiktokenEstimator`] guesses.
///
/// Returns `None` (rather than a zeroed [`AnthropicUsage`], which would
/// look like a real if empty call) when the row's `message` field is
/// absent (e.g. `TranscriptRow::User` rows sometimes carry `message: None`)
/// or when the present `message` has no `usage` sub-object at all — most
/// `user` rows, which never carry API usage.
#[must_use]
pub(crate) fn extract_call_usage(row: &TranscriptRow) -> Option<AnthropicUsage> {
    let message = row.fields().message.as_ref()?;
    crate::providers::extract_usage(message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_row(message: &serde_json::Value) -> TranscriptRow {
        let raw = json!({
            "type": "assistant",
            "uuid": "a1",
            "parentUuid": null,
            "isSidechain": false,
            "isMeta": false,
            "message": message,
        });
        serde_json::from_value(raw).unwrap()
    }

    fn user_row_with_message(message: Option<serde_json::Value>) -> TranscriptRow {
        let mut raw = json!({
            "type": "user",
            "uuid": "u1",
            "parentUuid": null,
            "isSidechain": false,
            "isMeta": false,
        });
        if let Some(message) = message {
            raw["message"] = message;
        }
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn extract_call_usage_should_return_full_usage_when_message_has_usage_object() {
        let row = assistant_row(&json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 340,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 15000,
            }
        }));

        assert_eq!(
            extract_call_usage(&row),
            Some(AnthropicUsage {
                input_tokens: 1200,
                output_tokens: 340,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 15000,
            })
        );
    }

    #[test]
    fn extract_call_usage_should_return_none_when_message_missing_usage_key() {
        let row = assistant_row(&json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
        }));

        assert_eq!(extract_call_usage(&row), None);
    }

    #[test]
    fn extract_call_usage_should_return_none_when_message_field_missing() {
        let row = user_row_with_message(None);

        assert_eq!(extract_call_usage(&row), None);
    }

    #[test]
    fn extract_call_usage_should_return_none_for_user_row() {
        let row = user_row_with_message(Some(json!({
            "role": "user",
            "content": "hello",
        })));

        assert_eq!(extract_call_usage(&row), None);
    }
}
