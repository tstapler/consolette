//! Per-session route override store (in-memory only) and the request-body
//! session-id extraction it keys off of.
//!
//! Session identity comes from the Anthropic Messages API request's
//! `metadata.user_id` field, used verbatim as the key rather than parsed
//! for an assumed internal structure — the exact shape Claude Code's CLI
//! puts there hasn't been directly captured against a live request through
//! this proxy (no session was pointed at it during development of this
//! feature). Verify the field's actual value for a real session via
//! `GET /requests/{id}` (the cached original body) before relying on this
//! for anything beyond "some client sent a stable `metadata.user_id`".
//! Absent the field entirely, no override can apply and dispatch falls
//! through to the normal route — this fails safe either way.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One session's pinned upstream (by config name) and optional model
/// override, set via `POST /api/sessions/{id}/route`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOverride {
    pub upstream: String,
    pub model: Option<String>,
}

/// Extracts `body.metadata.user_id` verbatim as the session key. Returns
/// `None` if the request has no `metadata.user_id` string field, in which
/// case no session-scoped override can ever apply to it.
#[must_use]
pub fn extract_session_id(body: &serde_json::Value) -> Option<String> {
    body.get("metadata")?
        .get("user_id")?
        .as_str()
        .map(str::to_string)
}

/// In-memory session-id -> override map. Deliberately not persisted to
/// disk: a pin is scoped to one session's lifetime, unlike the global route
/// (`runtime-overrides.toml`), which is meant to survive a restart.
#[derive(Default)]
pub struct SessionOverrideStore {
    overrides: Mutex<HashMap<String, SessionOverride>>,
}

impl SessionOverrideStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, session_id: String, over: SessionOverride) {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, over);
    }

    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<SessionOverride> {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    /// Removes a session's override, if any was set. Returns whether one
    /// was actually removed.
    pub fn clear(&self, session_id: &str) -> bool {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id)
            .is_some()
    }

    /// Snapshot of every currently-set override, for `GET /api/sessions`.
    #[must_use]
    pub fn list(&self) -> HashMap<String, SessionOverride> {
        self.overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
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

    #[test]
    fn set_get_clear_roundtrip() {
        let store = SessionOverrideStore::new();
        assert_eq!(store.get("s1"), None);

        store.set(
            "s1".to_string(),
            SessionOverride {
                upstream: "bedrock".to_string(),
                model: Some("m".to_string()),
            },
        );
        assert_eq!(
            store.get("s1"),
            Some(SessionOverride {
                upstream: "bedrock".to_string(),
                model: Some("m".to_string()),
            })
        );
        assert_eq!(store.list().len(), 1);

        assert!(store.clear("s1"));
        assert_eq!(store.get("s1"), None);
        assert!(!store.clear("s1"), "clearing twice must not report success");
    }
}
