//! `PlanReinjection` / `SkillReinjection`: restore the active plan and
//! in-use tool/skill names after a summarization pass drops the turn that
//! originally established them.
//!
//! This is the component the design doc calls "genuinely new state, not a
//! transform of existing state" — it reads from
//! [`crate::session_compaction::session_state::SessionState`] rather than
//! deriving everything from the current request body, since the whole point
//! is remembering something a *later* summarization pass may have erased.
//!
//! Plan/skill *detection* (what counts as "the active plan") is scoped
//! narrowly here: a plan is any `text` block whose content starts with the
//! literal marker `PLAN:` (mirroring how `crate::session_compaction`'s
//! callers would mark one — this proxy has no plan-tracking convention of
//! its own to reuse yet), and skills are the set of `tool_use` block names
//! seen in a turn. Recognizing richer plan formats is left to whoever wires
//! this into a real request path, once that convention exists.

use serde_json::{json, Value};

use super::session_state::SessionState;

pub const PLAN_MARKER_PREFIX: &str = "PLAN:";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReinjectionStats {
    pub plan_reinjected: bool,
    pub skills_reinjected: usize,
}

/// Scan `messages` for the latest plan marker and the set of tool/skill
/// names used, so callers can keep `SessionState` current before it's
/// needed for reinjection.
#[must_use]
pub fn extract_plan_and_skills(messages: &Value) -> (Option<String>, Vec<String>) {
    let Some(arr) = messages.as_array() else {
        return (None, Vec::new());
    };

    let mut latest_plan = None;
    let mut skills = Vec::new();

    for message in arr {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if let Some(plan) = text.strip_prefix(PLAN_MARKER_PREFIX) {
                            latest_plan = Some(plan.trim().to_string());
                        }
                    }
                }
                Some("tool_use") => {
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        if !skills.contains(&name.to_string()) {
                            skills.push(name.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    (latest_plan, skills)
}

/// Whether `text` appears verbatim anywhere in `messages`' content blocks.
fn contains_text(messages: &[Value], needle: &str) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains(needle))
                })
            })
    })
}

/// If `state.active_plan`/`active_skills` no longer appear anywhere in
/// `messages` (summarization dropped the turn that established them),
/// prepend a synthetic reminder message restoring them. No-op if both are
/// already present or unset.
#[must_use]
pub fn reinject_if_missing(messages: &Value, session: &SessionState) -> (Value, ReinjectionStats) {
    let Some(arr) = messages.as_array() else {
        return (messages.clone(), ReinjectionStats::default());
    };

    let mut outcome = ReinjectionStats::default();
    let mut reminder_lines = Vec::new();

    if let Some(plan) = &session.active_plan {
        if !contains_text(arr, plan) {
            reminder_lines.push(format!("{PLAN_MARKER_PREFIX} {plan}"));
            outcome.plan_reinjected = true;
        }
    }

    let missing_skills: Vec<&String> = session
        .active_skills
        .iter()
        .filter(|skill| !contains_text(arr, skill))
        .collect();
    if !missing_skills.is_empty() {
        let names = missing_skills
            .iter()
            .map(|skill: &&String| skill.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        reminder_lines.push(format!("Active skills still in scope: {names}"));
        outcome.skills_reinjected = missing_skills.len();
    }

    if reminder_lines.is_empty() {
        return (messages.clone(), outcome);
    }

    let mut out = Vec::with_capacity(arr.len() + 1);
    out.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": reminder_lines.join("\n")}]
    }));
    out.extend(arr.iter().cloned());

    (Value::Array(out), outcome)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn text_msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn extracts_latest_plan_marker() {
        let messages = json!([
            text_msg("user", "PLAN: step one"),
            text_msg("assistant", "ok"),
            text_msg("user", "PLAN: step two, revised"),
        ]);
        let (plan, _skills) = extract_plan_and_skills(&messages);
        assert_eq!(plan.as_deref(), Some("step two, revised"));
    }

    #[test]
    fn extracts_unique_tool_use_names() {
        let messages = json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "search_files", "input": {}},
                {"type": "tool_use", "id": "t2", "name": "search_files", "input": {}},
                {"type": "tool_use", "id": "t3", "name": "read_file", "input": {}},
            ]}
        ]);
        let (_plan, skills) = extract_plan_and_skills(&messages);
        assert_eq!(skills, vec!["search_files", "read_file"]);
    }

    #[test]
    fn reinjects_dropped_plan() {
        let messages = json!([text_msg("assistant", "kept turn")]);
        let state = SessionState {
            active_plan: Some("finish the migration".to_string()),
            active_skills: Vec::new(),
            summarized_turn_count: 1,
        };
        let (out, outcome) = reinject_if_missing(&messages, &state);
        assert!(outcome.plan_reinjected);
        let text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("finish the migration"));
        assert_eq!(out[1], text_msg("assistant", "kept turn"));
    }

    #[test]
    fn does_not_reinject_plan_still_present() {
        let messages = json!([text_msg("user", "PLAN: finish the migration")]);
        let state = SessionState {
            active_plan: Some("finish the migration".to_string()),
            active_skills: Vec::new(),
            summarized_turn_count: 0,
        };
        let (out, outcome) = reinject_if_missing(&messages, &state);
        assert!(!outcome.plan_reinjected);
        assert_eq!(out, messages);
    }

    #[test]
    fn reinjects_missing_skills_only() {
        let messages = json!([text_msg("assistant", "search_files was used earlier")]);
        let state = SessionState {
            active_plan: None,
            active_skills: vec!["search_files".to_string(), "read_file".to_string()],
            summarized_turn_count: 1,
        };
        let (out, outcome) = reinject_if_missing(&messages, &state);
        assert_eq!(outcome.skills_reinjected, 1);
        let text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("read_file"));
        assert!(!text.contains("Active skills") || !text.contains("search_files,"));
    }

    #[test]
    fn no_op_when_nothing_active() {
        let messages = json!([text_msg("assistant", "kept turn")]);
        let (out, stats) = reinject_if_missing(&messages, &SessionState::default());
        assert_eq!(stats, ReinjectionStats::default());
        assert_eq!(out, messages);
    }
}
