//! Shared, provider-agnostic session-identity extraction.
//!
//! Lives outside both `routing` and `providers` so either layer can depend
//! on it without `providers` reaching into `routing` (a layering violation
//! that used to exist when `providers::gemini` imported
//! `crate::routing::session_overrides::extract_session_id` directly).

/// Extracts `body.metadata.user_id` verbatim as the session key. Returns
/// `None` if the request has no `metadata.user_id` string field, in which
/// case no session-scoped override can ever apply to it.
///
/// Session identity comes from the Anthropic Messages API request's
/// `metadata.user_id` field, used verbatim as the key rather than parsed
/// for an assumed internal structure — the exact shape Claude Code's CLI
/// puts there hasn't been directly captured against a live request through
/// this proxy (no session was pointed at it during development of this
/// feature). Verify the field's actual value for a real session via
/// `GET /requests/{id}` (the cached original body) before relying on this
/// for anything beyond "some client sent a stable `metadata.user_id`".
/// Absent the field entirely, no override can apply and dispatch falls
/// through to the normal route — this fails safe either way.
#[must_use]
pub fn extract_session_id(body: &serde_json::Value) -> Option<String> {
    body.get("metadata")?
        .get("user_id")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_session_id_reads_metadata_user_id() {
        let body = serde_json::json!({"metadata": {"user_id": "abc123"}});
        assert_eq!(extract_session_id(&body), Some("abc123".to_string()));
    }

    #[test]
    fn extract_session_id_missing_metadata_returns_none() {
        assert_eq!(extract_session_id(&serde_json::json!({})), None);
    }

    #[test]
    fn extract_session_id_non_string_user_id_returns_none() {
        let body = serde_json::json!({"metadata": {"user_id": 42}});
        assert_eq!(extract_session_id(&body), None);
    }
}
