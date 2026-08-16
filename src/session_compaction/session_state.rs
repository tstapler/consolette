//! Per-conversation state that survives across requests within one session —
//! the prerequisite the design doc calls "the real gap":
//! `project_plans/consolette/design/session-level-compaction.md`'s
//! "The real gap" section.
//!
//! `SessionKey` is left as an opaque caller-supplied string rather than
//! derived from real traffic here: the design doc flags "how consolette
//! recognizes this request continues that session" as an unresolved
//! verification spike (same category as issue #8's stateless request-header
//! check), and nothing in `src/routing`/`src/main.rs` wires an HTTP server up
//! to a client-observable identity signal yet. Callers (tests today, a future
//! router integration once that spike lands) own key derivation; this module
//! only owns what happens once a key exists.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use tokio::sync::RwLock;

/// Opaque per-conversation identity. See module docs for why this isn't
/// derived from request contents here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(pub String);

impl SessionKey {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        SessionKey(id.into())
    }
}

/// State `PlanReinjection`/`SkillReinjection`/`ConversationSummarizer` need
/// to remember between requests for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    /// Text of the most recently established plan, if any turn has set one.
    pub active_plan: Option<String>,
    /// Names of skills/tools whose defining turn may get summarized away.
    pub active_skills: Vec<String>,
    /// How many turns `ConversationSummarizer` has collapsed so far, used to
    /// avoid re-summarizing turns it already replaced with a boundary marker.
    pub summarized_turn_count: usize,
}

/// TTL-evicted store of `SessionState`, one entry per `SessionKey`.
///
/// Mirrors [`crate::compression::rewind::RewindStore`]'s `moka` cache shape.
/// TTL is longer than `RewindStore`'s (1 hour vs. 10 min) since session state
/// must outlive the gap between a user's turns, not just one in-flight
/// compress/retrieve round trip.
pub struct SessionStateStore {
    cache: Cache<SessionKey, Arc<RwLock<SessionState>>>,
}

impl SessionStateStore {
    #[allow(clippy::new_without_default, clippy::unused_async)]
    pub async fn new() -> Self {
        let cache = Cache::builder()
            .max_capacity(1000)
            .time_to_live(Duration::from_hours(1))
            .build();
        SessionStateStore { cache }
    }

    /// Fetch this session's state, creating an empty one if absent.
    pub async fn get_or_default(&self, key: &SessionKey) -> Arc<RwLock<SessionState>> {
        if let Some(existing) = self.cache.get(key).await {
            return existing;
        }
        let fresh = Arc::new(RwLock::new(SessionState::default()));
        self.cache.insert(key.clone(), Arc::clone(&fresh)).await;
        fresh
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_or_default_creates_empty_state_on_first_access() {
        let store = SessionStateStore::new().await;
        let key = SessionKey::new("session-1");
        let state = store.get_or_default(&key).await;
        assert_eq!(*state.read().await, SessionState::default());
    }

    #[tokio::test]
    async fn get_or_default_returns_same_state_across_calls() {
        let store = SessionStateStore::new().await;
        let key = SessionKey::new("session-1");

        let first = store.get_or_default(&key).await;
        first.write().await.active_plan = Some("do the thing".to_string());

        let second = store.get_or_default(&key).await;
        assert_eq!(
            second.read().await.active_plan.as_deref(),
            Some("do the thing")
        );
    }

    #[tokio::test]
    async fn different_keys_get_independent_state() {
        let store = SessionStateStore::new().await;
        let key_a = SessionKey::new("session-a");
        let key_b = SessionKey::new("session-b");

        store.get_or_default(&key_a).await.write().await.active_plan = Some("plan-a".to_string());

        let state_b = store.get_or_default(&key_b).await;
        assert_eq!(state_b.read().await.active_plan, None);
    }
}
