//! Configuration policy data model and concurrent store for transcript memory pruning.
//!
//! Provides [`PruningPolicy`], [`ToolPruningRule`], and [`PruningPolicyStore`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// Per-tool pruning rule with optional limit and decay overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolPruningRule {
    /// Glob pattern matching target tool names (e.g., `"Bash*"`, `"mcp__*"`, `"Agent"`).
    pub pattern: String,
    /// Optional character limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_chars: Option<usize>,
    /// Optional word limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_words: Option<usize>,
    /// Optional turn age decay threshold override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turn_age: Option<usize>,
    /// Optional unreferenced turn decay threshold override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreferenced_turn_decay: Option<usize>,
    /// Force pruning regardless of character/word thresholds if turn decay triggers.
    #[serde(default)]
    pub force_prune: bool,
}

impl ToolPruningRule {
    /// Returns `true` if `tool_name` matches the rule's glob pattern.
    #[must_use]
    pub fn matches(&self, tool_name: &str) -> bool {
        glob::Pattern::new(&self.pattern)
            .map_or_else(|_| tool_name == self.pattern, |p| p.matches(tool_name))
    }
}

/// Configuration policy for transcript memory pruning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PruningPolicy {
    /// Whether transcript pruning is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Default character threshold for unclassified tools.
    #[serde(default = "default_limit_chars")]
    pub default_limit_chars: usize,
    /// Default word threshold for unclassified tools.
    #[serde(default = "default_limit_words")]
    pub default_limit_words: usize,
    /// Maximum turn age before output is pruned (if set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turn_age: Option<usize>,
    /// Unreferenced turn decay window before unreferenced output is pruned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreferenced_turn_decay: Option<usize>,
    /// Maximum total tool context bytes before LRU eviction triggers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_context_bytes: Option<usize>,
    /// Number of recent trailing turns protected from pruning (default: 2).
    #[serde(default = "default_preserve_recent_turns")]
    pub preserve_recent_turns: usize,
    /// Whether tool outputs resulting in errors should be preserved (default: true).
    #[serde(default = "default_true")]
    pub preserve_error_outputs: bool,
    /// Specific rules for tool name patterns (evaluated in order).
    #[serde(default)]
    pub tool_rules: Vec<ToolPruningRule>,
}

const fn default_true() -> bool {
    true
}

const fn default_limit_chars() -> usize {
    1024
}

const fn default_limit_words() -> usize {
    128
}

const fn default_preserve_recent_turns() -> usize {
    2
}

impl Default for PruningPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            default_limit_chars: 1024,
            default_limit_words: 128,
            max_turn_age: None,
            unreferenced_turn_decay: None,
            max_tool_context_bytes: None,
            preserve_recent_turns: 2,
            preserve_error_outputs: true,
            tool_rules: vec![
                ToolPruningRule {
                    pattern: "Bash".to_string(),
                    limit_chars: Some(1024),
                    limit_words: None,
                    max_turn_age: Some(5),
                    unreferenced_turn_decay: None,
                    force_prune: false,
                },
                ToolPruningRule {
                    pattern: "Agent".to_string(),
                    limit_chars: Some(4096),
                    limit_words: Some(512),
                    max_turn_age: Some(10),
                    unreferenced_turn_decay: None,
                    force_prune: false,
                },
                ToolPruningRule {
                    pattern: "TaskOutput".to_string(),
                    limit_chars: Some(4096),
                    limit_words: Some(512),
                    max_turn_age: Some(10),
                    unreferenced_turn_decay: None,
                    force_prune: false,
                },
            ],
        }
    }
}

impl PruningPolicy {
    /// Finds the first rule in `tool_rules` matching `tool_name`.
    #[must_use]
    pub fn find_matching_rule(&self, tool_name: &str) -> Option<&ToolPruningRule> {
        self.tool_rules.iter().find(|rule| rule.matches(tool_name))
    }
}

/// Thread-safe policy store managing global defaults and session-specific policy overrides.
#[derive(Debug, Default)]
pub struct PruningPolicyStore {
    global_policy: RwLock<PruningPolicy>,
    session_overrides: RwLock<HashMap<String, PruningPolicy>>,
}

impl PruningPolicyStore {
    /// Creates a new `PruningPolicyStore` with the specified global policy.
    #[must_use]
    pub fn new(global_policy: PruningPolicy) -> Self {
        Self {
            global_policy: RwLock::new(global_policy),
            session_overrides: RwLock::new(HashMap::new()),
        }
    }

    /// Gets the effective pruning policy for a session.
    ///
    /// If `session_id` is provided and has a session override registered,
    /// that override policy is returned. Otherwise, the global default policy is returned.
    #[must_use]
    pub fn get_policy(&self, session_id: Option<&str>) -> PruningPolicy {
        if let Some(sid) = session_id {
            let overrides = self
                .session_overrides
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(policy) = overrides.get(sid) {
                return policy.clone();
            }
        }
        let global = self
            .global_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        global.clone()
    }

    /// Sets or updates the policy override for a specific session ID.
    pub fn set_session_policy(&self, session_id: impl Into<String>, policy: PruningPolicy) {
        let mut overrides = self
            .session_overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        overrides.insert(session_id.into(), policy);
    }

    /// Sets or updates the global default pruning policy.
    pub fn set_global_policy(&self, policy: PruningPolicy) {
        let mut global = self
            .global_policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *global = policy;
    }

    /// Removes a session-specific policy override.
    pub fn clear_session_policy(&self, session_id: &str) {
        let mut overrides = self
            .session_overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        overrides.remove(session_id);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// UT-POLICY-001: `PruningPolicy` struct default creation and JSON serde serialization/deserialization.
    #[test]
    fn policy_defaults_and_serde_round_trip() {
        let policy = PruningPolicy::default();
        assert!(policy.enabled);
        assert_eq!(policy.default_limit_chars, 1024);
        assert_eq!(policy.default_limit_words, 128);
        assert_eq!(policy.preserve_recent_turns, 2);
        assert!(policy.preserve_error_outputs);
        assert_eq!(policy.tool_rules.len(), 3);

        let json = serde_json::to_string(&policy).unwrap();
        let deserialized: PruningPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, deserialized);
    }

    /// UT-POLICY-002: Glob pattern evaluation using `glob::Pattern` for tool names (`Bash`, `Agent`, `mcp__*`, `TaskOutput`, `Bash*`, `Read`).
    #[test]
    fn tool_pruning_rule_glob_matching() {
        let rule_mcp = ToolPruningRule {
            pattern: "mcp__*".to_string(),
            limit_chars: Some(2048),
            limit_words: None,
            max_turn_age: None,
            unreferenced_turn_decay: None,
            force_prune: false,
        };
        assert!(rule_mcp.matches("mcp__fetch"));
        assert!(rule_mcp.matches("mcp__github_get_issue"));
        assert!(!rule_mcp.matches("Bash"));
        assert!(!rule_mcp.matches("mcp"));

        let rule_bash_wildcard = ToolPruningRule {
            pattern: "Bash*".to_string(),
            limit_chars: Some(1024),
            limit_words: None,
            max_turn_age: Some(5),
            unreferenced_turn_decay: None,
            force_prune: false,
        };
        assert!(rule_bash_wildcard.matches("Bash"));
        assert!(rule_bash_wildcard.matches("BashCommand"));
        assert!(!rule_bash_wildcard.matches("Agent"));

        let policy = PruningPolicy::default();
        assert!(policy.find_matching_rule("Bash").is_some());
        assert_eq!(
            policy.find_matching_rule("Bash").unwrap().limit_chars,
            Some(1024)
        );
        assert!(policy.find_matching_rule("Agent").is_some());
        assert_eq!(
            policy.find_matching_rule("Agent").unwrap().limit_chars,
            Some(4096)
        );
        assert!(policy.find_matching_rule("TaskOutput").is_some());
        assert!(policy.find_matching_rule("Read").is_none());
    }

    /// UT-POLICY-003: `PruningPolicyStore` thread-safety under concurrent readers/writers and per-session policy override fallback to global defaults.
    #[test]
    fn policy_store_session_overrides_and_concurrency() {
        let store = Arc::new(PruningPolicyStore::default());

        // Baseline global check
        let default_policy = store.get_policy(None);
        assert_eq!(default_policy.default_limit_chars, 1024);

        let session_policy_1 = store.get_policy(Some("session-1"));
        assert_eq!(session_policy_1.default_limit_chars, 1024);

        // Set session override
        let custom_policy = PruningPolicy {
            default_limit_chars: 8192,
            ..PruningPolicy::default()
        };
        store.set_session_policy("session-1", custom_policy.clone());

        assert_eq!(
            store.get_policy(Some("session-1")).default_limit_chars,
            8192
        );
        // Fallback to global for another session
        assert_eq!(
            store.get_policy(Some("session-2")).default_limit_chars,
            1024
        );

        // Update global policy
        let new_global = PruningPolicy {
            default_limit_chars: 2048,
            ..PruningPolicy::default()
        };
        store.set_global_policy(new_global);

        // Session 1 still has override
        assert_eq!(
            store.get_policy(Some("session-1")).default_limit_chars,
            8192
        );
        // Session 2 gets new global default
        assert_eq!(
            store.get_policy(Some("session-2")).default_limit_chars,
            2048
        );

        // Clear session override
        store.clear_session_policy("session-1");
        assert_eq!(
            store.get_policy(Some("session-1")).default_limit_chars,
            2048
        );

        // Concurrent access test
        let mut handles = Vec::new();
        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            let handle = thread::spawn(move || {
                let sid = format!("thread-session-{i}");
                let mut policy = store_clone.get_policy(Some(&sid));
                policy.default_limit_chars = 1000 + i;
                store_clone.set_session_policy(&sid, policy);

                let retrieved = store_clone.get_policy(Some(&sid));
                assert_eq!(retrieved.default_limit_chars, 1000 + i);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
