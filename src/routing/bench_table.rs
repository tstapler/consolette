//! Static coding-benchmark ranking table for `OpenRouter` free models
//! (Epic 4.1). Feeds the `bench` term of the composite scoring formula
//! (`0.5·error + 0.3·latency + 0.2·bench`, ADR-003) via [`bench_score`];
//! this module does not implement the scoring itself (Epic 4.2).
//!
//! Override path: edit this table's rows and rebuild consolette. There is
//! no runtime/config-file override — see
//! `project_plans/openrouter-routing/requirements.md` Scope for why this is
//! a deliberate simplification, not a gap (static-default + manual-edit was
//! chosen over building a leaderboard scraper).
//!
//! # Data source
//! Source: <https://aider.chat/docs/leaderboards/> (aider-polyglot
//! benchmark, Apache-2.0), retrieved 2026-09-07.
//!
//! # Coverage ratio (Pre-mortem P2 #1 go/no-go, recorded 2026-09-07)
//! Cross-referencing the leaderboard against a live
//! `GET https://openrouter.ai/api/v1/models` capture on 2026-09-07 (the same
//! capture Task 1.2.2a confirmed — 19 of 428 models priced
//! `pricing.prompt == "0"`), **zero of those 19 free models have a
//! same-family match on the aider-polyglot leaderboard**: the live free
//! lineup that day was `inclusionai/ling-3.0-flash-{sante,fin}:free`,
//! `dots-studio/dots-3-note-preview:free`, `liquid/lfm-2.5-2.6b:free`,
//! `nvidia/nemotron-3.5-lightning:free`,
//! `nvidia/nemotron-3.5-content-safety:free`,
//! `nvidia/nemotron-3-ultra-550b-a55b:free`,
//! `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free`,
//! `nvidia/nemotron-3-super-120b-a12b:free`,
//! `thinkingmachines/inkling{,-small}:free`,
//! `poolside/laguna-{s,xs}-2.1:free`, `cohere/north-mini-code:free`,
//! `google/gemma-4-{26b-a4b,31b}-it:free`, `google/lyria-3-{pro,clip}-preview`
//! (audio/music, not code), and `openrouter/free` — none of these
//! families (or their nearest-named prior generation, e.g. `gemma-3` vs.
//! `gemma-4`) appear in the leaderboard's rows, and inventing a
//! cross-generation score for them would be fabrication, not transcription.
//!
//! **Coverage ratio: 0/19 (0.0%) of the live free-model snapshot.** Every
//! candidate in that snapshot falls back to the neutral `0.5` default in
//! the composite scorer (Epic 4.2) until the table is manually refreshed
//! against a later, more leaderboard-covered free lineup. TODO(Tyler): this
//! is a residual-risk item, not silently absorbed — per this plan's
//! Unresolved Questions, a 0% ratio this low means Epic 4.2 should either
//! lower `WEIGHT_BENCH` or add an aggregate startup log line surfacing this
//! ratio (implementation decision left to Epic 4.2, not made here).
//!
//! The one row below is *not* one of the 2026-09-07 live free models — it
//! is this plan's own worked example (Story 4.1.1 Acceptance Criteria) and
//! a genuine transcription of the leaderboard's "`DeepSeek` V3 (0324)" row
//! (55.1%), kept so the table and its tests exercise real, sourced data
//! rather than shipping fully empty. It will keep resolving via
//! `bench_score` if/when a `deepseek/deepseek-chat-v3.1:free`-family model
//! reappears in `OpenRouter`'s free lineup.
pub const BENCH_TABLE: &[(&str, f64)] = &[("deepseek/deepseek-chat-v3.1:free", 55.1)];

/// Looks up `model_id`'s aider-polyglot pass rate in [`BENCH_TABLE`],
/// returning it normalized to `[0.0, 1.0]`. Fails soft: a model absent from
/// the table (the common case today, see the module doc's coverage ratio)
/// returns `None` rather than a fabricated value — callers apply their own
/// neutral default (ADR-003: `0.5`).
///
/// The table is small enough that a linear scan is preferable to the
/// bookkeeping of a `HashMap`.
#[must_use]
pub fn bench_score(model_id: &str) -> Option<f64> {
    BENCH_TABLE
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, pass_rate)| pass_rate / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_score_should_return_pass_rate_for_known_model() {
        assert_eq!(bench_score("deepseek/deepseek-chat-v3.1:free"), Some(0.551));
    }

    #[test]
    fn bench_score_should_return_none_for_unranked_model() {
        assert_eq!(bench_score("some/unranked-model:free"), None);
    }
}
