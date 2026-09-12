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

/// Extracts the session key verbatim: `body.metadata.user_id` first, then
/// the OpenAI-native top-level `user` string as fallback (the opencode path
/// carries the session key in one of these two shapes; see Epic 4's adapter
/// carry-through in `translate_openai_to_anthropic`). Returns `None` if
/// neither is a string field, in which case no session-scoped override can
/// ever apply to the request.
#[must_use]
pub fn extract_session_id(body: &serde_json::Value) -> Option<String> {
    body.get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            body.get("user")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// Re-evaluation cadence for auto-stickiness (STICKY-PER-SESSION, confirmed
/// value): a stuck session re-resolves after this many sticky serves with no
/// health event. Implementer-tunable via
/// [`SessionOverrideStore::with_sticky_every`].
pub const STICKY_REEVALUATE_EVERY: u64 = 50;

/// One session's auto-stuck family pick: the `(upstream, model)` the session
/// resolved to, plus how many requests have served it since the stick formed.
/// An explicit [`SessionOverride`] (pin/move) always wins over this at
/// dispatch; the stick only applies to unpinned family-alias requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyPick {
    pub upstream: String,
    pub model: String,
    /// Sticky serves so far including the resolving request. Valid while
    /// `served < sticky_every`; the request arriving at `served ==
    /// sticky_every` re-resolves instead (so K=50 serves exactly 50 sticky
    /// requests before re-evaluating).
    pub served: u64,
}

/// In-memory session-id -> override map. Deliberately not persisted to
/// disk: a pin is scoped to one session's lifetime, unlike the global route
/// (`runtime-overrides.toml`), which is meant to survive a restart.
///
/// Also holds the auto-stickiness table (Epic 4 Story 4.2): per
/// `(session, alias)` stuck picks recorded on first family resolution.
/// Explicit pins and auto-sticks live in separate maps so `clear()` (the
/// `DELETE /api/sessions/{id}/route` path) only drops the explicit pin —
/// a stale stick is still a valid family pick and stays within its K-window
/// semantics — while dispatch consults pins first either way.
pub struct SessionOverrideStore {
    overrides: Mutex<HashMap<String, SessionOverride>>,
    sticky: Mutex<HashMap<(String, String), StickyPick>>,
    sticky_every: u64,
}

impl Default for SessionOverrideStore {
    fn default() -> Self {
        Self {
            overrides: Mutex::new(HashMap::new()),
            sticky: Mutex::new(HashMap::new()),
            sticky_every: STICKY_REEVALUATE_EVERY,
        }
    }
}

impl SessionOverrideStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the stickiness cadence (default [`STICKY_REEVALUATE_EVERY`]).
    /// Test/operator tuning only; production uses the default.
    #[must_use]
    pub fn with_sticky_every(mut self, sticky_every: u64) -> Self {
        self.sticky_every = sticky_every.max(1);
        self
    }

    /// The active stickiness cadence (see [`STICKY_REEVALUATE_EVERY`]).
    #[must_use]
    pub fn sticky_every(&self) -> u64 {
        self.sticky_every
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

    // ── Auto-stickiness (Epic 4 Story 4.2; STICKY-PER-SESSION) ──

    /// Records (or re-records) a session's stuck pick for `alias`, resetting
    /// its serve count. Called on a fresh family resolution; skipped on
    /// exploration probes (a session must never stick to a sampled member).
    pub fn sticky_record(&self, session_id: &str, alias: &str, upstream: &str, model: &str) {
        self.sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (session_id.to_string(), alias.to_string()),
                StickyPick {
                    upstream: upstream.to_string(),
                    model: model.to_string(),
                    served: 1,
                },
            );
    }

    /// The session's stuck pick for `alias`, if one was recorded.
    /// Freshness (K-window, cooldown/exclusion invalidation) is decided by
    /// the dispatch caller, which owns the health/runtime reads.
    #[must_use]
    pub fn sticky_lookup(&self, session_id: &str, alias: &str) -> Option<StickyPick> {
        self.sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(session_id.to_string(), alias.to_string()))
            .cloned()
    }

    /// Notes one more sticky serve. Returns the new count.
    pub fn sticky_note_served(&self, session_id: &str, alias: &str) -> u64 {
        let mut sticky = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = sticky.get_mut(&(session_id.to_string(), alias.to_string())) {
            entry.served += 1;
            entry.served
        } else {
            0
        }
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
    fn extract_session_id_should_return_none_when_metadata_missing() {
        // R6 error row: an opencode-shaped body carrying neither
        // `metadata.user_id` nor the OpenAI-native `user` field yields no
        // session key, so no pin applies and family resolution proceeds.
        // (This row documents the dropped-key failure mode Epic 4 fixes via
        // the adapter carry-through; the cases below must stay `None`.)
        assert_eq!(extract_session_id(&serde_json::json!({})), None);
        assert_eq!(
            extract_session_id(&serde_json::json!({"model": "auto-coding"})),
            None
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({"metadata": {"user_id": 42}})),
            None
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({
                "model": "auto-coding",
                "messages": [{"role": "user", "content": "hi"}],
            })),
            None
        );
        // Positive controls: both key shapes resolve.
        assert_eq!(
            extract_session_id(&serde_json::json!({"metadata": {"user_id": "s1"}})),
            Some("s1".to_string())
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({"user": "s1"})),
            Some("s1".to_string())
        );
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
