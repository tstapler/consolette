//! Per-session route override store (in-memory only). The request-body
//! session-id extraction it keys off of lives in `crate::session` (shared
//! across `routing` and `providers` so neither layer has to reach into the
//! other); re-exported here so existing call sites in `routing/` keep
//! working unchanged.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub use crate::session::extract_session_id;

/// One session's pinned upstream (by config name) and optional model
/// override, set via `POST /api/sessions/{id}/route`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOverride {
    pub upstream: String,
    pub model: Option<String>,
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
