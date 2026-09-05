//! Gemini tool-call bookkeeping: `ToolUseId`, `GeminiToolCallState`,
//! `ThoughtSignatureCache` (Story 1.6.1 onward).
//!
//! `ToolUseId` and `ThoughtSignatureCache` were scaffolded here in Phase 1
//! (Story 1.6.1) ahead of their first real use in Phase 3 (Story 3.3.1), so
//! both the request-direction `GeminiToolCallState` (added in Story 3.2.1)
//! and the cross-call `ThoughtSignatureCache` share one key type instead of
//! each reinventing a raw-`String`-keyed map. See
//! `project_plans/gemini-provider/implementation/plan.md`'s Epic 1.6 for the
//! full rationale, especially why the cache must be a `GeminiProvider`-owned
//! field keyed by `(session_key, ToolUseId)` rather than a per-`send()`-call
//! local keyed by `ToolUseId` alone.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Newtype wrapping an Anthropic `tool_use` block's `id` string. Used
/// consistently as the key type in both `GeminiToolCallState` (Phase 3) and
/// `ThoughtSignatureCache`, mirroring the existing typed-id precedent
/// `cost_metrics::types::RequestId` — a raw `String` key/value pair at a call
/// site like `insert(id: &str, signature: String)` gives the compiler
/// nothing to catch an accidental argument swap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ToolUseId(String);

impl From<String> for ToolUseId {
    fn from(value: String) -> Self {
        ToolUseId(value)
    }
}

impl AsRef<str> for ToolUseId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Request-scoped `ToolUseId` -> Gemini function-name lookup (Story 3.2.1),
/// built by walking one Anthropic request's `messages[]` in translation
/// order and by registering ids synthesized while translating one Gemini
/// response back to Anthropic shape (Story 3.2.2).
///
/// Deliberately NOT `GeminiProvider`-owned/persisted like
/// `ThoughtSignatureCache`: Anthropic's wire format re-sends the full
/// conversation history on every request, including each earlier `tool_use`
/// block's `name` right alongside its `id`, so a fresh instance built inside
/// each `translate_anthropic_request_to_gemini` call always has everything
/// it needs from that call's own `messages[]` walk — no cross-request
/// state-smuggling required.
#[derive(Debug, Default)]
pub(crate) struct GeminiToolCallState {
    by_tool_use_id: std::collections::HashMap<ToolUseId, String>,
}

impl GeminiToolCallState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records `id -> function_name`, overwriting any prior mapping for the
    /// same id (matching how a re-sent `tool_use` block would be handled).
    pub(crate) fn insert(&mut self, id: ToolUseId, function_name: String) {
        self.by_tool_use_id.insert(id, function_name);
    }

    /// Looks up the function name registered for `id`, if any.
    pub(crate) fn get(&self, id: &ToolUseId) -> Option<&str> {
        self.by_tool_use_id.get(id).map(String::as_str)
    }
}

/// How long a stashed `thought_signature` is trusted before it's considered
/// stale and swept. 900s (15 min) comfortably outlives a normal
/// request/tool-result turnaround without letting the cache grow unbounded
/// across a long-lived `GeminiProvider` instance.
const THOUGHT_SIGNATURE_TTL_SECS: u64 = 900;

struct CacheEntry {
    signature: String,
    inserted_at: Instant,
}

/// `GeminiProvider`-owned, concurrency-safe cache of Gemini 3 Pro's opaque
/// per-`functionCall` `thought_signature`, so it can be echoed back unmodified
/// on a later, separate HTTP request (Story 3.3.1). Backed by a `DashMap`
/// mirroring `ExecCredentialCache`'s pattern (`src/auth/exec.rs:52-55`).
///
/// Keyed by `(session_key, ToolUseId)`, not `ToolUseId` alone: `GeminiProvider`
/// is one shared `Arc<dyn Provider>` serving every concurrent conversation
/// routed to "gemini" (`Provider::send(&self, ..)`), so a bare `ToolUseId` key
/// would let two unrelated concurrent conversations cross-wire signatures. See
/// plan.md's Epic 1.6 "Session-key design decision" for how `session_key` is
/// derived and its documented residual risk.
///
/// Currently never populated or read (no tool calls exist until Phase 3) —
/// only its lifetime (provider-owned, not `send()`-call-local) matters in
/// Phase 1.
pub(crate) struct ThoughtSignatureCache {
    entries: DashMap<(String, ToolUseId), CacheEntry>,
}

impl ThoughtSignatureCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Inserts `signature` under `(session_key, id)`, first sweeping any
    /// entry older than `THOUGHT_SIGNATURE_TTL_SECS` (sweep-on-insert, no
    /// separate background task needed at this traffic scale).
    pub(crate) fn insert(&self, session_key: &str, id: ToolUseId, signature: String) {
        let ttl = Duration::from_secs(THOUGHT_SIGNATURE_TTL_SECS);
        self.entries
            .retain(|_, entry| entry.inserted_at.elapsed() < ttl);

        self.entries.insert(
            (session_key.to_string(), id),
            CacheEntry {
                signature,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Looks up a previously-stashed signature for `(session_key, id)`.
    /// Returns `None` for a different session key even with the same `id` —
    /// see the module doc comment on `ThoughtSignatureCache`.
    pub(crate) fn get(&self, session_key: &str, id: &ToolUseId) -> Option<String> {
        self.entries
            .get(&(session_key.to_string(), id.clone()))
            .map(|entry| entry.signature.clone())
    }

    /// Test-only: current entry count, for asserting TTL sweep/eviction
    /// behavior without exposing cache internals outside `#[cfg(test)]`.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // Story 3.2.1 (Task 3.2.1a) — GeminiToolCallState insert/get round-trip.
    #[test]
    fn gemini_tool_call_state_get_should_return_registered_function_name_for_known_id() {
        let mut state = GeminiToolCallState::new();
        let id = ToolUseId::from("toolu_01".to_string());

        state.insert(id.clone(), "get_weather".to_string());

        assert_eq!(state.get(&id), Some("get_weather"));
    }

    // Story 3.2.1 (Task 3.2.1a) — unknown id looks up to None rather than
    // panicking, matching the fail-closed lookup used by translate.rs.
    #[test]
    fn gemini_tool_call_state_get_should_return_none_for_unregistered_id() {
        let state = GeminiToolCallState::new();
        let unknown = ToolUseId::from("toolu_unknown".to_string());

        assert_eq!(state.get(&unknown), None);
    }

    // REQ-13 (Story 1.6.1) — pure-`tools.rs` unit test: a different session
    // key never sees an entry inserted under another session key, even for
    // the identical `ToolUseId`.
    #[test]
    fn thought_signature_cache_get_should_not_see_entry_inserted_under_a_different_session_key() {
        let cache = ThoughtSignatureCache::new();
        let id = ToolUseId::from("toolu_01".to_string());

        cache.insert("session-b", id.clone(), "sig-for-b".to_string());

        assert_eq!(cache.get("session-a", &id), None);
        assert_eq!(cache.get("session-b", &id), Some("sig-for-b".to_string()));
    }

    // REQ-24 (Story 3.3.1, Task 3.3.1d) — happy path: a stash under
    // `(session_key, ToolUseId)` using the synthesized `tool_use` id shape
    // (`toolu_{uuid}`, Story 3.2.2) round-trips through `get`.
    #[test]
    fn thought_signature_cache_insert_should_stash_signature_keyed_by_session_and_synthesized_tool_use_id(
    ) {
        let cache = ThoughtSignatureCache::new();
        let synthesized_id = ToolUseId::from(format!("toolu_{}", uuid::Uuid::new_v4()));

        cache.insert(
            "session-a",
            synthesized_id.clone(),
            "opaque-blob-xyz".to_string(),
        );

        assert_eq!(
            cache.get("session-a", &synthesized_id),
            Some("opaque-blob-xyz".to_string())
        );
    }

    // REQ-24 (Story 3.3.1, Task 3.3.1d) — THE specific cross-conversation
    // isolation test named in the task brief: identical `ToolUseId`,
    // different `session_key`, must never cross-wire.
    #[test]
    fn thought_signature_cache_get_should_return_none_when_session_key_differs_even_with_identical_tool_use_id(
    ) {
        let cache = ThoughtSignatureCache::new();
        let id = ToolUseId::from("toolu_abc123".to_string());

        cache.insert("session-a", id.clone(), "sig-for-a".to_string());

        assert_eq!(cache.get("session-b", &id), None);
    }

    // REQ-13 (Story 1.6.1) — TTL sweep: an entry older than
    // THOUGHT_SIGNATURE_TTL_SECS is gone after the next `.insert()` call.
    //
    // Made deterministic by constructing a `CacheEntry` directly with a
    // backdated `Instant` (`Instant::now() - (TTL + slack)`) rather than
    // sleeping in the test or introducing an injectable/shortened TTL just
    // for testability — `Instant` supports subtraction directly, so this
    // needs no new production-code seam.
    #[test]
    fn thought_signature_cache_insert_should_evict_entries_older_than_ttl_on_next_insert() {
        let cache = ThoughtSignatureCache::new();
        let stale_id = ToolUseId::from("toolu_stale".to_string());
        let fresh_id = ToolUseId::from("toolu_fresh".to_string());

        let backdated = Instant::now()
            .checked_sub(Duration::from_secs(THOUGHT_SIGNATURE_TTL_SECS + 60))
            .expect("test host uptime too short to backdate an Instant");
        cache.entries.insert(
            ("session-a".to_string(), stale_id.clone()),
            CacheEntry {
                signature: "stale-sig".to_string(),
                inserted_at: backdated,
            },
        );
        assert_eq!(cache.len(), 1);

        // Any insert sweeps stale entries first, including one for an
        // unrelated key.
        cache.insert("session-a", fresh_id.clone(), "fresh-sig".to_string());

        assert_eq!(cache.get("session-a", &stale_id), None);
        assert_eq!(
            cache.get("session-a", &fresh_id),
            Some("fresh-sig".to_string())
        );
        assert_eq!(cache.len(), 1);
    }
}
