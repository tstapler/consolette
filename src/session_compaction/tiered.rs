//! `TieredCompaction`: select a compaction tier from context-pressure %.
//!
//! Unlike the cross-request `SessionState` most of
//! `project_plans/consolette/design/session-level-compaction.md` assumes is a
//! prerequisite, tier selection needs no persisted state: the Anthropic
//! Messages API is stateless per request and the client resends the full
//! `messages[]` array every turn, so pressure can be computed fresh from the
//! request body that's already in hand. This module is a pure function of
//! that pressure percentage plus config; it holds no state of its own.

use serde::{Deserialize, Serialize};

/// Compaction tier selected for a given context-pressure percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CompactionTier {
    /// Below the micro threshold: no compaction runs.
    Off,
    /// `ToolResultBudget` only — cheapest, no summarization, reversible.
    Micro,
    /// Micro + `ConversationSummarizer` on turns older than a keep-window.
    Auto,
    /// Auto with a smaller keep-window, re-running Plan/Skill reinjection
    /// afterward to restore anything summarization dropped.
    Full,
}

/// Pressure thresholds (0.0-1.0) at which each tier engages. Each threshold
/// must be less than the next; `tier_for_pressure` treats a malformed config
/// (thresholds out of order) as if every tier below the misordered one were
/// unreachable, since it evaluates from `full` down to `micro`.
#[derive(Debug, Clone, Copy)]
pub struct TierThresholds {
    pub micro: f32,
    pub auto: f32,
    pub full: f32,
}

impl Default for TierThresholds {
    fn default() -> Self {
        // Defaults chosen per the design doc's example values; expected to
        // move to `Config` (figment, ADR-001) once this is wired to a live
        // request path.
        TierThresholds {
            micro: 0.60,
            auto: 0.75,
            full: 0.90,
        }
    }
}

/// Select a tier for the given context-pressure percentage (0.0-1.0).
///
/// Evaluated highest tier first so a pressure value at or above `full` always
/// resolves to `Full` even if `auto`/`micro` are misconfigured to be higher.
#[must_use]
pub fn tier_for_pressure(pressure_pct: f32, thresholds: &TierThresholds) -> CompactionTier {
    if pressure_pct >= thresholds.full {
        CompactionTier::Full
    } else if pressure_pct >= thresholds.auto {
        CompactionTier::Auto
    } else if pressure_pct >= thresholds.micro {
        CompactionTier::Micro
    } else {
        CompactionTier::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_micro_threshold_is_off() {
        let t = TierThresholds::default();
        assert_eq!(tier_for_pressure(0.10, &t), CompactionTier::Off);
        assert_eq!(tier_for_pressure(0.59, &t), CompactionTier::Off);
    }

    #[test]
    fn at_micro_threshold_is_micro() {
        let t = TierThresholds::default();
        assert_eq!(tier_for_pressure(0.60, &t), CompactionTier::Micro);
        assert_eq!(tier_for_pressure(0.74, &t), CompactionTier::Micro);
    }

    #[test]
    fn at_auto_threshold_is_auto() {
        let t = TierThresholds::default();
        assert_eq!(tier_for_pressure(0.75, &t), CompactionTier::Auto);
        assert_eq!(tier_for_pressure(0.89, &t), CompactionTier::Auto);
    }

    #[test]
    fn at_full_threshold_and_above_is_full() {
        let t = TierThresholds::default();
        assert_eq!(tier_for_pressure(0.90, &t), CompactionTier::Full);
        assert_eq!(tier_for_pressure(1.0, &t), CompactionTier::Full);
    }

    #[test]
    fn tiers_are_ordered() {
        assert!(CompactionTier::Off < CompactionTier::Micro);
        assert!(CompactionTier::Micro < CompactionTier::Auto);
        assert!(CompactionTier::Auto < CompactionTier::Full);
    }
}
