//! `BudgetThreshold` — a newtype wrapping the four context-window budget
//! presets the growth-chart dashboard (Story 1.4.3) toggles between
//! (context-analyzer plan.md Domain Glossary).
//!
//! Newtype over a raw `u64` (Pattern Decision: type-driven-design) so a
//! plain token count can never be confused with a budget threshold, and so
//! `crossed()` has a clear receiver instead of comparison logic scattered
//! across dashboard/query code.

use serde::{Deserialize, Serialize};

/// A context-window budget threshold, in tokens — one of the four presets
/// the dashboard offers as toggle buttons, or a custom value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BudgetThreshold(pub u64);

impl BudgetThreshold {
    pub const K200: BudgetThreshold = BudgetThreshold(200_000);
    pub const K500: BudgetThreshold = BudgetThreshold(500_000);
    pub const K700: BudgetThreshold = BudgetThreshold(700_000);
    pub const M1: BudgetThreshold = BudgetThreshold(1_000_000);

    /// The four dashboard presets, in ascending order.
    pub const PRESETS: [BudgetThreshold; 4] = [
        BudgetThreshold::K200,
        BudgetThreshold::K500,
        BudgetThreshold::K700,
        BudgetThreshold::M1,
    ];

    /// `true` when `peak_tokens` meets or exceeds this threshold.
    #[must_use]
    pub fn crossed(self, peak_tokens: u64) -> bool {
        peak_tokens >= self.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_threshold_crossed_should_return_true_when_peak_tokens_exceeds_preset() {
        assert!(BudgetThreshold::K500.crossed(600_000));
    }

    #[test]
    fn budget_threshold_crossed_should_return_true_when_peak_tokens_exactly_equals_preset() {
        assert!(BudgetThreshold::K500.crossed(500_000));
    }

    #[test]
    fn budget_threshold_crossed_should_return_false_when_peak_tokens_below_preset() {
        assert!(!BudgetThreshold::K500.crossed(499_999));
    }

    #[test]
    fn presets_should_be_in_ascending_order() {
        let values: Vec<u64> = BudgetThreshold::PRESETS.iter().map(|t| t.0).collect();
        let mut sorted = values.clone();
        sorted.sort_unstable();
        assert_eq!(values, sorted);
    }
}
