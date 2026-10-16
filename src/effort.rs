//! Reasoning-effort model: the client's requested level, operator policy, and a per-model
//! capability table. Pure data and functions; no request rewriting yet (see
//! `project_plans/effort-mapping/research-and-design.md`). A leaf module: it must not depend on
//! `config`, which already depends on it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Anthropic `output_config.effort` levels, ordered weakest to strongest.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Level {
    pub const ALL: [Level; 5] = [
        Level::Low,
        Level::Medium,
        Level::High,
        Level::Xhigh,
        Level::Max,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
            Level::Xhigh => "xhigh",
            Level::Max => "max",
        }
    }

    /// Case-insensitive; `None` for anything else, since clients may send levels newer than
    /// this table and an unknown level must not fail the request. Config files use the serde
    /// derive instead, which accepts only the lowercase names.
    #[must_use]
    pub fn parse(s: &str) -> Option<Level> {
        Level::ALL
            .into_iter()
            .find(|l| l.as_str().eq_ignore_ascii_case(s.trim()))
    }

    /// Nearest level for a legacy `thinking.budget_tokens`, by the ceilings in
    /// [`BUDGET_CEILINGS`].
    #[must_use]
    pub fn from_budget_tokens(budget: u64) -> Level {
        BUDGET_CEILINGS
            .iter()
            .find(|(ceiling, _)| budget <= *ceiling)
            .map_or(Level::Max, |(_, level)| *level)
    }
}

/// Inclusive upper bound of `budget_tokens` per level; inverts the LiteLLM level-to-budget table
/// (1024/2048/4096/8192/16384). Anything above the last ceiling is `Max`.
const BUDGET_CEILINGS: [(u64, Level); 4] = [
    (1024, Level::Low),
    (2048, Level::Medium),
    (4096, Level::High),
    (8192, Level::Xhigh),
];

/// The client's `thinking` request field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    Adaptive,
    Enabled { budget_tokens: Option<u64> },
    Disabled,
}

/// Effort-related fields of an inbound Anthropic-shaped request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientEffort {
    /// `output_config.effort`, when present and recognized.
    pub level: Option<Level>,
    pub thinking: Option<ThinkingMode>,
}

impl ClientEffort {
    #[must_use]
    pub fn from_request(body: &Value) -> Self {
        let level = body
            .pointer("/output_config/effort")
            .and_then(Value::as_str)
            .and_then(Level::parse);
        let thinking =
            body.get("thinking")
                .and_then(|t| match t.get("type").and_then(Value::as_str)? {
                    "adaptive" => Some(ThinkingMode::Adaptive),
                    "enabled" => Some(ThinkingMode::Enabled {
                        budget_tokens: t.get("budget_tokens").and_then(Value::as_u64),
                    }),
                    "disabled" => Some(ThinkingMode::Disabled),
                    _ => None,
                });
        Self { level, thinking }
    }

    /// The explicit level, else one derived from a legacy thinking budget.
    #[must_use]
    pub fn effective_level(&self) -> Option<Level> {
        self.level.or(match self.thinking {
            Some(ThinkingMode::Enabled {
                budget_tokens: Some(b),
            }) => Some(Level::from_budget_tokens(b)),
            _ => None,
        })
    }
}

/// Per-upstream effort handling. `Off` forwards nothing, which is today's behavior.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EffortMode {
    #[default]
    Off,
    Map,
}

/// Operator policy applied to the client's level. `force` wins over the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffortPolicy {
    pub force: Option<Level>,
    pub min: Option<Level>,
    pub max: Option<Level>,
}

impl EffortPolicy {
    /// `None` when neither the policy nor the client names a level: silence stays silence.
    /// `min`/`max` clamp whichever level results (`max` wins if the bounds were inverted).
    #[must_use]
    pub fn resolve(&self, client: Option<Level>) -> Option<Level> {
        let level = self.force.or(client)?;
        let level = self.min.map_or(level, |min| level.max(min));
        Some(self.max.map_or(level, |max| level.min(max)))
    }
}

/// How a model accepts the `thinking` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSupport {
    /// `{type:"adaptive"}` only; the legacy budget form is a 400.
    AdaptiveOnly,
    /// `{type:"enabled", budget_tokens}` only; `adaptive` is a 400.
    EnabledOnly,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    /// Supported effort levels, ascending. Empty means the model takes no effort field.
    pub levels: &'static [Level],
    pub thinking: ThinkingSupport,
}

impl ModelCaps {
    /// Highest supported level not above `requested`; if `requested` is below every supported
    /// level, the lowest supported one. `None` when the model takes no effort field.
    #[must_use]
    pub fn clamp(&self, requested: Level) -> Option<Level> {
        self.levels
            .iter()
            .rev()
            .find(|l| **l <= requested)
            .or_else(|| self.levels.first())
            .copied()
    }
}

const NO_XHIGH: &[Level] = &[Level::Low, Level::Medium, Level::High, Level::Max];

/// Anthropic models by normalized-id prefix; longest match wins. Sourced from the effort and
/// thinking docs (2026-10-16, see the research doc). Opus 4.5 supports effort but the docs read
/// don't list its levels, so it is left empty (effort omitted) until confirmed.
const ANTHROPIC_MODELS: &[(&str, ModelCaps)] = &[
    (
        "claude-fable-5",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-mythos-5",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-opus-5",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-opus-4-8",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-opus-4-7",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-sonnet-5",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    (
        "claude-haiku-5",
        caps(&Level::ALL, ThinkingSupport::AdaptiveOnly),
    ),
    ("claude-opus-4-6", caps(NO_XHIGH, ThinkingSupport::Both)),
    ("claude-sonnet-4-6", caps(NO_XHIGH, ThinkingSupport::Both)),
    ("claude-opus-4-5", caps(&[], ThinkingSupport::EnabledOnly)),
    ("claude-sonnet-4-5", caps(&[], ThinkingSupport::EnabledOnly)),
    ("claude-haiku-4-5", caps(&[], ThinkingSupport::EnabledOnly)),
];

const fn caps(levels: &'static [Level], thinking: ThinkingSupport) -> ModelCaps {
    ModelCaps { levels, thinking }
}

/// Reduces provider-decorated ids to the bare Claude id: `us.anthropic.claude-opus-4-8-v1:0`,
/// `anthropic/claude-opus-4.8` and `claude-opus-4-8-20260101` all become a string starting
/// `claude-opus-4-8`.
fn normalize_claude_id(model: &str) -> Option<String> {
    let lower = model.trim().to_lowercase();
    let start = lower.find("claude-")?;
    Some(lower[start..].replace('.', "-"))
}

/// Capabilities of a known Anthropic model, `None` for anything unrecognized (callers must then
/// leave effort and thinking untouched).
#[must_use]
pub fn anthropic_caps(model: &str) -> Option<ModelCaps> {
    let id = normalize_claude_id(model)?;
    ANTHROPIC_MODELS
        .iter()
        .filter(|(prefix, _)| {
            // A digit after the prefix means a different version (`...4-8` vs `...4-80`).
            id.strip_prefix(prefix)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, caps)| *caps)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Request fields as captured from Claude Code 2.1.296 through a local listener (2026-10-16).
    fn captured(effort: &str) -> Value {
        json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64000,
            "output_config": {"effort": effort},
            "thinking": {"type": "adaptive", "display": "omitted"},
            "messages": [],
        })
    }

    #[test]
    fn from_request_should_read_captured_claude_code_shape() {
        let got = ClientEffort::from_request(&captured("xhigh"));
        assert_eq!(got.level, Some(Level::Xhigh));
        assert_eq!(got.thinking, Some(ThinkingMode::Adaptive));
    }

    #[test]
    fn from_request_should_ignore_unknown_level_and_missing_fields() {
        let got = ClientEffort::from_request(&captured("ludicrous"));
        assert_eq!(got.level, None);
        assert_eq!(
            ClientEffort::from_request(&json!({})),
            ClientEffort::default()
        );
    }

    #[test]
    fn effective_level_should_derive_from_legacy_budget() {
        let body = json!({"thinking": {"type": "enabled", "budget_tokens": 4096}});
        let got = ClientEffort::from_request(&body);
        assert_eq!(got.level, None);
        assert_eq!(got.effective_level(), Some(Level::High));
        let explicit = json!({"output_config": {"effort": "low"},
            "thinking": {"type": "enabled", "budget_tokens": 31999}});
        assert_eq!(
            ClientEffort::from_request(&explicit).effective_level(),
            Some(Level::Low)
        );
    }

    #[test]
    fn from_budget_tokens_should_follow_litellm_thresholds() {
        let cases = [
            (1, Level::Low),
            (1024, Level::Low),
            (1025, Level::Medium),
            (2048, Level::Medium),
            (2049, Level::High),
            (4096, Level::High),
            (4097, Level::Xhigh),
            (8192, Level::Xhigh),
            (8193, Level::Max),
            (31999, Level::Max),
        ];
        for (budget, want) in cases {
            assert_eq!(Level::from_budget_tokens(budget), want, "budget {budget}");
        }
    }

    #[test]
    fn level_parse_should_be_case_insensitive_and_roundtrip() {
        for level in Level::ALL {
            assert_eq!(Level::parse(level.as_str()), Some(level));
        }
        assert_eq!(Level::parse(" HIGH "), Some(Level::High));
        assert_eq!(Level::parse("minimal"), None);
    }

    #[test]
    fn resolve_should_prefer_force_then_clamp_to_bounds() {
        let policy = EffortPolicy {
            force: Some(Level::Max),
            min: None,
            max: Some(Level::High),
        };
        assert_eq!(policy.resolve(Some(Level::Low)), Some(Level::High));
        let bounded = EffortPolicy {
            force: None,
            min: Some(Level::Medium),
            max: Some(Level::High),
        };
        assert_eq!(bounded.resolve(Some(Level::Low)), Some(Level::Medium));
        assert_eq!(bounded.resolve(Some(Level::Max)), Some(Level::High));
        assert_eq!(bounded.resolve(Some(Level::High)), Some(Level::High));
    }

    #[test]
    fn resolve_should_let_max_win_when_bounds_are_inverted() {
        let inverted = EffortPolicy {
            force: None,
            min: Some(Level::High),
            max: Some(Level::Low),
        };
        assert_eq!(inverted.resolve(Some(Level::Medium)), Some(Level::Low));
    }

    #[test]
    fn resolve_should_stay_silent_without_a_level() {
        let bounded = EffortPolicy {
            force: None,
            min: Some(Level::Medium),
            max: None,
        };
        assert_eq!(bounded.resolve(None), None);
        let forced = EffortPolicy {
            force: Some(Level::Low),
            ..EffortPolicy::default()
        };
        assert_eq!(forced.resolve(None), Some(Level::Low));
    }

    #[test]
    fn anthropic_caps_should_normalize_decorated_ids() {
        for id in [
            "claude-opus-4-8",
            "claude-opus-4.8",
            "Claude-Opus-4-8-20260101",
            "us.anthropic.claude-opus-4-8-v1:0",
            "anthropic/claude-opus-4.8",
        ] {
            let caps = anthropic_caps(id).unwrap_or_else(|| panic!("{id} should be known"));
            assert_eq!(caps.levels, &Level::ALL, "{id}");
            assert_eq!(caps.thinking, ThinkingSupport::AdaptiveOnly, "{id}");
        }
    }

    #[test]
    fn anthropic_caps_should_distinguish_adjacent_versions() {
        let old = anthropic_caps("claude-opus-4-6").expect("4.6 known");
        assert!(!old.levels.contains(&Level::Xhigh));
        assert_eq!(old.thinking, ThinkingSupport::Both);
        let legacy = anthropic_caps("claude-sonnet-4-5-20250929").expect("4.5 known");
        assert_eq!(legacy.thinking, ThinkingSupport::EnabledOnly);
        assert!(legacy.levels.is_empty());
        assert!(anthropic_caps("claude-opus-4-1").is_none());
    }

    #[test]
    fn anthropic_caps_should_cover_every_table_row_and_respect_version_boundaries() {
        use ThinkingSupport::{AdaptiveOnly, Both, EnabledOnly};
        let cases: [(&str, usize, ThinkingSupport); 11] = [
            ("claude-fable-5-1", 5, AdaptiveOnly),
            ("claude-mythos-5", 5, AdaptiveOnly),
            ("claude-opus-5-5", 5, AdaptiveOnly),
            ("claude-opus-4-7", 5, AdaptiveOnly),
            ("claude-sonnet-5-5", 5, AdaptiveOnly),
            ("claude-haiku-5-5", 5, AdaptiveOnly),
            ("claude-opus-4-6", 4, Both),
            ("claude-sonnet-4-6", 4, Both),
            ("claude-opus-4-5", 0, EnabledOnly),
            ("claude-sonnet-4-5", 0, EnabledOnly),
            ("claude-opus-4-8[1m]", 5, AdaptiveOnly),
        ];
        for (id, levels, thinking) in cases {
            let caps = anthropic_caps(id).unwrap_or_else(|| panic!("{id} should be known"));
            assert_eq!(caps.levels.len(), levels, "{id}");
            assert_eq!(caps.thinking, thinking, "{id}");
        }
        assert!(anthropic_caps("claude-opus-4-80").is_none());
    }

    #[test]
    fn anthropic_caps_should_return_none_for_non_claude_models() {
        for id in ["gpt-5.6", "gemini-3-pro", "", "llama-4"] {
            assert!(anthropic_caps(id).is_none(), "{id}");
        }
    }

    #[test]
    fn clamp_should_step_down_to_the_highest_supported_level() {
        let no_xhigh = anthropic_caps("claude-opus-4-6").expect("known");
        assert_eq!(no_xhigh.clamp(Level::Xhigh), Some(Level::High));
        assert_eq!(no_xhigh.clamp(Level::Max), Some(Level::Max));
        let none = anthropic_caps("claude-haiku-4-5").expect("known");
        assert_eq!(none.clamp(Level::High), None);
        let all = anthropic_caps("claude-sonnet-5-5").expect("known");
        assert_eq!(all.clamp(Level::Xhigh), Some(Level::Xhigh));
    }
}
