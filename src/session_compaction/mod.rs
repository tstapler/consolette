//! Session-level message-list compaction ([tstapler/consolette#7]).
//!
//! See `project_plans/consolette/design/session-level-compaction.md` for the
//! full design. `TieredCompaction` and `ToolResultBudget` need no persisted
//! state — the Anthropic Messages API is stateless per request and
//! `messages[]` already carries the full conversation history on every
//! call — but `ConversationSummarizer` needs to track how much it's already
//! collapsed, and `PlanReinjection`/`SkillReinjection` need to remember what
//! was established in turns that summarization may since have dropped. Both
//! live in [`session_state::SessionState`], keyed by an opaque
//! [`session_state::SessionKey`] the caller supplies — deriving that key
//! from real client traffic is flagged in the design doc as an unresolved
//! verification spike, not solved here.
//!
//! [`SessionCompactionPipeline`] is the entry point: it selects a tier from
//! context-pressure, runs `CompactHooks::pre_compact`, applies
//! `ToolResultBudget` (and, at higher tiers, `ConversationSummarizer` and
//! reinjection), updates `SessionState`, and runs `CompactHooks::post_compact`.
//!
//! [tstapler/consolette#7]: https://github.com/tstapler/consolette/issues/7

pub mod hooks;
pub mod reinjection;
pub mod session_state;
pub mod summarizer;
pub mod tiered;
pub mod tool_result_budget;

pub use hooks::{CompactHookRegistry, CompactHooks, CompactionReport};
pub use reinjection::{extract_plan_and_skills, reinject_if_missing, ReinjectionStats};
pub use session_state::{SessionKey, SessionState, SessionStateStore};
pub use summarizer::{summarize_older_turns, SummarizerStats};
pub use tiered::{tier_for_pressure, CompactionTier, TierThresholds};
pub use tool_result_budget::{budget_tool_results, ToolResultBudgetStats};

use serde_json::Value;

/// How many `tool_result` blocks each tier keeps verbatim before eliding
/// older ones. `Off` keeps everything (no-op); the tighter tiers keep less.
#[must_use]
fn keep_recent_tool_results_for_tier(tier: CompactionTier) -> Option<usize> {
    match tier {
        CompactionTier::Off => None,
        CompactionTier::Micro => Some(10),
        CompactionTier::Auto => Some(6),
        CompactionTier::Full => Some(3),
    }
}

/// How many trailing messages `ConversationSummarizer` keeps verbatim at
/// `Auto`/`Full`. `Full` uses a smaller window per the design doc ("Auto,
/// with a smaller keep-window").
#[must_use]
fn keep_recent_turns_for_tier(tier: CompactionTier) -> Option<usize> {
    match tier {
        CompactionTier::Off | CompactionTier::Micro => None,
        CompactionTier::Auto => Some(12),
        CompactionTier::Full => Some(6),
    }
}

/// Ties `TieredCompaction`, `ToolResultBudget`, `ConversationSummarizer`,
/// `PlanReinjection`/`SkillReinjection`, and `CompactHooks` together against
/// one session's persisted state.
pub struct SessionCompactionPipeline {
    store: SessionStateStore,
    hooks: CompactHookRegistry,
    thresholds: TierThresholds,
}

impl SessionCompactionPipeline {
    pub async fn new(thresholds: TierThresholds) -> Self {
        SessionCompactionPipeline {
            store: SessionStateStore::new().await,
            hooks: CompactHookRegistry::new(),
            thresholds,
        }
    }

    pub fn register_hook(&mut self, hook: std::sync::Arc<dyn CompactHooks>) {
        self.hooks.register(hook);
    }

    /// Run the full pipeline for `session_key` over `messages` at the given
    /// context-pressure percentage (0.0-1.0). Returns the (possibly
    /// rewritten) messages array and a report of what ran.
    pub async fn apply(
        &self,
        session_key: &SessionKey,
        messages: &Value,
        pressure_pct: f32,
    ) -> (Value, CompactionReport) {
        let tier = tier_for_pressure(pressure_pct, &self.thresholds);
        let session_lock = self.store.get_or_default(session_key).await;

        {
            let state = session_lock.read().await;
            self.hooks.run_pre_compact(&state);
        }

        let mut report = CompactionReport {
            tier: Some(tier),
            ..CompactionReport::default()
        };

        // Micro and above: age-budget tool results.
        let mut out = messages.clone();
        if let Some(keep_recent) = keep_recent_tool_results_for_tier(tier) {
            let (budgeted, stats) = budget_tool_results(&out, keep_recent);
            out = budgeted;
            report.tool_result_stats = stats;
        }

        // Auto and above: summarize turns older than the tier's keep-window.
        if let Some(keep_recent) = keep_recent_turns_for_tier(tier) {
            let (plan, skills) = extract_plan_and_skills(&out);
            {
                let mut state = session_lock.write().await;
                if plan.is_some() {
                    state.active_plan = plan;
                }
                for skill in skills {
                    if !state.active_skills.contains(&skill) {
                        state.active_skills.push(skill);
                    }
                }
            }

            let (summarized, summary_stats) = summarize_older_turns(&out, keep_recent);
            out = summarized;
            report.summarizer_stats = summary_stats;

            // Full: reinject anything the summarization pass dropped.
            if tier == CompactionTier::Full {
                let (reinjected, reinjection_stats) = {
                    let session = session_lock.read().await;
                    reinject_if_missing(&out, &session)
                };
                out = reinjected;
                let mut session = session_lock.write().await;
                session.summarized_turn_count += summary_stats.turns_summarized;
                drop(session);
                let _ = reinjection_stats; // surfaced via out; stats struct kept minimal on CompactionReport for now
            } else {
                let mut session = session_lock.write().await;
                session.summarized_turn_count += summary_stats.turns_summarized;
            }
        }

        {
            let state = session_lock.read().await;
            self.hooks.run_post_compact(&state, &report);
        }

        (out, report)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_result_messages(n: usize) -> Value {
        let msgs: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": format!("result {i}")}
                    ]
                })
            })
            .collect();
        Value::Array(msgs)
    }

    #[tokio::test]
    async fn off_tier_is_a_no_op() {
        let pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
        let messages = tool_result_messages(20);
        let key = SessionKey::new("s1");
        let (out, report) = pipeline.apply(&key, &messages, 0.10).await;
        assert_eq!(report.tier, Some(CompactionTier::Off));
        assert_eq!(report.tool_result_stats.elided_count, 0);
        assert_eq!(out, messages);
    }

    #[tokio::test]
    async fn full_tier_runs_budget_and_summarizer() {
        let pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
        let messages = tool_result_messages(20);
        let key = SessionKey::new("s2");
        let (out, report) = pipeline.apply(&key, &messages, 0.95).await;
        assert_eq!(report.tier, Some(CompactionTier::Full));
        assert!(report.tool_result_stats.elided_count > 0);
        assert!(report.summarizer_stats.turns_summarized > 0);
        // Boundary marker prepended plus the kept recent tail.
        assert!(out.as_array().unwrap().len() < messages.as_array().unwrap().len());
    }

    #[tokio::test]
    async fn full_tier_reinjects_plan_dropped_by_summarization() {
        let pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
        let key = SessionKey::new("s3");

        let mut msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "PLAN: ship the feature"}]
        })];
        for i in 0..20 {
            msgs.push(json!({
                "role": if i % 2 == 0 { "assistant" } else { "user" },
                "content": [{"type": "text", "text": format!("filler {i}")}]
            }));
        }
        let messages = Value::Array(msgs);

        let (out, _report) = pipeline.apply(&key, &messages, 0.95).await;
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(serialized.contains("ship the feature"));
    }

    #[tokio::test]
    async fn hooks_fire_around_apply() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(Default)]
        struct CountingHook(AtomicUsize, AtomicUsize);
        impl CompactHooks for CountingHook {
            fn pre_compact(&self, _session: &SessionState) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn post_compact(&self, _session: &SessionState, _report: &CompactionReport) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
        let hook = Arc::new(CountingHook::default());
        pipeline.register_hook(hook.clone());

        let key = SessionKey::new("s4");
        let messages = json!([]);
        pipeline.apply(&key, &messages, 0.10).await;

        assert_eq!(hook.0.load(Ordering::SeqCst), 1);
        assert_eq!(hook.1.load(Ordering::SeqCst), 1);
    }
}
