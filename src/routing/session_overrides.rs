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

use std::collections::{HashMap, VecDeque};
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

/// Cap on tracked sessions per map. Both maps key on client-controlled
/// session IDs, so without a bound distinct-ID traffic grows them without
/// limit. Past the cap, inserting a NEW key first evicts the oldest-tracked
/// key (insertion-order deques approximate oldest-first; exact LRU isn't
/// worth the bookkeeping — pin/stick state is ephemeral and disposable, and
/// an evicted session simply re-resolves on its next request).
pub const MAX_SESSIONS: usize = 4096;

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
    /// Insertion order for the pin map, parallel to `overrides` — the
    /// [`MAX_SESSIONS`] bound evicts from the front.
    override_order: Mutex<VecDeque<String>>,
    sticky: Mutex<HashMap<(String, String), StickyPick>>,
    /// Insertion order for the sticky map, parallel to `sticky` — the
    /// [`MAX_SESSIONS`] bound evicts from the front.
    sticky_order: Mutex<VecDeque<(String, String)>>,
    sticky_every: u64,
}

impl Default for SessionOverrideStore {
    fn default() -> Self {
        Self {
            overrides: Mutex::new(HashMap::new()),
            override_order: Mutex::new(VecDeque::new()),
            sticky: Mutex::new(HashMap::new()),
            sticky_order: Mutex::new(VecDeque::new()),
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
        let mut overrides = self
            .overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !overrides.contains_key(&session_id) {
            // New session past the bound: evict the oldest-tracked pin
            // first so client-controlled IDs can't grow the map without
            // limit. Re-setting an existing session never evicts.
            let mut order = self
                .override_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while overrides.len() >= MAX_SESSIONS {
                let Some(oldest) = order.pop_front() else {
                    break;
                };
                if overrides.remove(&oldest).is_some() {
                    break;
                }
            }
            order.push_back(session_id.clone());
        }
        overrides.insert(session_id, over);
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
    /// A new session past [`MAX_SESSIONS`] evicts the oldest-tracked stick
    /// first (see [`set`](Self::set)); re-recording an existing stick never
    /// evicts.
    pub fn sticky_record(&self, session_id: &str, alias: &str, upstream: &str, model: &str) {
        let key = (session_id.to_string(), alias.to_string());
        let mut sticky = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !sticky.contains_key(&key) {
            let mut order = self
                .sticky_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while sticky.len() >= MAX_SESSIONS {
                let Some(oldest) = order.pop_front() else {
                    break;
                };
                if sticky.remove(&oldest).is_some() {
                    break;
                }
            }
            order.push_back(key.clone());
        }
        sticky.insert(
            key,
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

    /// Single-lock sticky check-and-increment: returns the stuck pick after
    /// noting one more serve, or `None` when no pick is recorded or the
    /// K-window already lapsed. The window re-check + bump happen under ONE
    /// lock hold, so concurrent dispatches can't overshoot `sticky_every`
    /// (a `sticky_lookup` hint beforehand is fine — this is the enforcing
    /// gate; dispatch uses it instead of lookup+`sticky_note_served`).
    #[must_use]
    pub fn try_sticky_serve(&self, session_id: &str, alias: &str) -> Option<StickyPick> {
        let mut sticky = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = sticky.get_mut(&(session_id.to_string(), alias.to_string()))?;
        if entry.served >= self.sticky_every {
            return None;
        }
        entry.served += 1;
        Some(entry.clone())
    }

    /// Live entry counts, for the [`MAX_SESSIONS`] bound tests.
    #[cfg(test)]
    fn len_for_test(&self) -> (usize, usize) {
        (
            self.overrides
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            self.sticky
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
        )
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

    #[test]
    fn try_sticky_serve_should_hold_exact_k_window() {
        // The recording request counts as serve 1; with K=3 exactly two
        // more single-lock serves succeed, then the window lapses.
        let store = SessionOverrideStore::new().with_sticky_every(3);
        store.sticky_record("s1", "auto-coding", "mock-a", "model-a:free");
        assert_eq!(
            store.sticky_lookup("s1", "auto-coding").map(|s| s.served),
            Some(1)
        );
        assert_eq!(
            store
                .try_sticky_serve("s1", "auto-coding")
                .map(|s| s.served),
            Some(2)
        );
        assert_eq!(
            store
                .try_sticky_serve("s1", "auto-coding")
                .map(|s| s.served),
            Some(3)
        );
        assert_eq!(store.try_sticky_serve("s1", "auto-coding"), None);
        assert_eq!(
            store.sticky_lookup("s1", "auto-coding").map(|s| s.served),
            Some(3),
            "a lapsed serve must not bump the count"
        );
        assert_eq!(
            store.try_sticky_serve("unknown", "auto-coding"),
            None,
            "no recorded pick must serve nothing"
        );
    }

    #[test]
    fn session_maps_should_stay_bounded_on_distinct_ids() {
        let store = SessionOverrideStore::new();
        for i in 0..MAX_SESSIONS + 50 {
            store.set(
                format!("session-{i}"),
                SessionOverride {
                    upstream: "mock".to_string(),
                    model: None,
                },
            );
            store.sticky_record(
                &format!("session-{i}"),
                "auto-coding",
                "mock",
                "model-a:free",
            );
        }
        let (overrides, sticky) = store.len_for_test();
        assert_eq!(
            overrides, MAX_SESSIONS,
            "pin map must be capped at MAX_SESSIONS"
        );
        assert_eq!(
            sticky, MAX_SESSIONS,
            "sticky map must be capped at MAX_SESSIONS"
        );
        // Re-setting / re-recording existing keys must not evict or grow.
        store.set(
            format!("session-{}", MAX_SESSIONS + 49),
            SessionOverride {
                upstream: "mock".to_string(),
                model: None,
            },
        );
        assert_eq!(store.len_for_test().0, MAX_SESSIONS);
    }
}
