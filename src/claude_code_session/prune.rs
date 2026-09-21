//! Per-tool-name pruning of bulky, completed tool I/O into
//! `src/claude_code_session/omission_cache.rs`, replacing the row's content
//! with a placeholder that embeds the resulting `content_id`.
//!
//! Evaluates multi-criteria policy rules (turn age decay, unreferenced eviction,
//! tool glob patterns, error output preservation, and trailing turn protection)
//! and enforces transcript capacity bounds (LRU/LRR eviction pass).

use crate::claude_code_session::omission_cache::OmissionCache;
pub use crate::claude_code_session::prune_policy::{
    PruningPolicy, PruningPolicyStore, ToolPruningRule,
};
use crate::claude_code_session::transcript::{
    build_row_turn_indices, relative_turn_age, ReferenceMap, RowFields, ToolNameMap, TranscriptRow,
    Turn,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

/// Default char threshold for tools with no more specific override.
pub const DEFAULT_LIMIT_CHARS: usize = 1024;
/// Default word threshold for tools with no more specific override.
pub const DEFAULT_LIMIT_WORDS: usize = 128;
/// Char threshold for the "agent output" tool class (`Agent`, `TaskOutput`).
pub const AGENT_OUTPUT_LIMIT_CHARS: usize = 4096;
/// Word threshold for the "agent output" tool class.
pub const AGENT_OUTPUT_LIMIT_WORDS: usize = 512;
/// Flat char cutoff for `Bash` tool output.
pub const BASH_LIMIT_CHARS: usize = 1024;

/// The reason why a tool output row was pruned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneReason {
    SizeThreshold,
    TurnAge,
    UnreferencedDecay,
    CapacityLru,
}

/// Breakdown of rows pruned by reason during a pruning pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReasonBreakdown {
    pub size_threshold: usize,
    pub turn_age: usize,
    pub unreferenced_decay: usize,
    pub capacity_lru: usize,
}

/// Detailed report of a pruning execution pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneExecutionReport {
    pub session_id: String,
    pub rows_evaluated: usize,
    pub rows_pruned: usize,
    pub bytes_freed: usize,
    pub estimated_tokens_saved: usize,
    pub pruned_by_reason: PruneReasonBreakdown,
    pub dry_run: bool,
}

/// Statistics collected during a transcript pruning pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruningStats {
    pub rows_checked: usize,
    pub rows_pruned: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub estimated_tokens_saved: usize,
    pub token_savings: usize,
    pub pruned_by_reason: PruneReasonBreakdown,
}

impl PruningStats {
    /// Calculate bytes freed.
    #[must_use]
    pub fn bytes_freed(&self) -> usize {
        self.bytes_before.saturating_sub(self.bytes_after)
    }

    /// Recalculate estimated tokens saved (`bytes_freed / 4`).
    pub fn update_totals(&mut self) {
        let freed = self.bytes_freed();
        self.estimated_tokens_saved = freed / 4;
        self.token_savings = self.estimated_tokens_saved;
    }
}

/// The outcome of [`prune_tool_row`] or [`prune_tool_row_with_policy`].
#[derive(Debug, Clone, PartialEq)]
pub enum PrunedRow {
    Unchanged(TranscriptRow),
    Pruned {
        row: TranscriptRow,
        content_id: String,
        reason: PruneReason,
    },
}

/// Rebuild `row` with the same enum variant but different [`RowFields`].
fn with_fields(row: &TranscriptRow, fields: RowFields) -> TranscriptRow {
    match row {
        TranscriptRow::User(_) => TranscriptRow::User(fields),
        TranscriptRow::Assistant(_) => TranscriptRow::Assistant(fields),
        TranscriptRow::System(_) => TranscriptRow::System(fields),
        TranscriptRow::Unknown(_) => TranscriptRow::Unknown(fields),
    }
}

/// Flatten a `tool_result` block's nested `content` field into plain text.
fn tool_result_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let blocks = content.as_array()?;
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        let plain_text: String = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if plain_text.is_empty() {
            None
        } else {
            Some(plain_text)
        }
    } else {
        Some(text)
    }
}

/// Extract `(tool_name, content_text)` from a `tool_result`-carrier row's `message` JSON.
pub fn extract_tool_result_with_map(
    row: &TranscriptRow,
    tool_name_map: Option<&ToolNameMap>,
) -> Option<(String, String)> {
    let message = row.fields().message.as_ref()?;
    let content = message.get("content")?;

    match content {
        Value::String(s) => Some(("unknown".to_string(), s.clone())),
        Value::Array(blocks) => {
            let mut tool_name: Option<String> = None;
            let mut texts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                if tool_name.is_none() {
                    tool_name = block
                        .get("tool_name")
                        .or_else(|| block.get("toolName"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            let tool_use_id = block.get("tool_use_id").and_then(Value::as_str)?;
                            tool_name_map?.get(tool_use_id).map(str::to_owned)
                        });
                }
                if let Some(inner) = block.get("content") {
                    if let Some(text) = tool_result_text(inner) {
                        texts.push(text);
                    }
                }
            }
            if texts.is_empty() {
                None
            } else {
                Some((
                    tool_name.unwrap_or_else(|| "unknown".to_string()),
                    texts.join("\n"),
                ))
            }
        }
        _ => None,
    }
}

fn extract_tool_result(row: &TranscriptRow) -> Option<(String, String)> {
    extract_tool_result_with_map(row, None)
}

/// `true` if `row` contains a `tool_result` block with `"is_error": true`.
#[must_use]
pub fn extract_is_error(row: &TranscriptRow) -> bool {
    let Some(message) = row.fields().message.as_ref() else {
        return false;
    };
    let Some(content) = message.get("content") else {
        return false;
    };
    let Some(blocks) = content.as_array() else {
        return false;
    };
    blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
            && (block.get("is_error").and_then(Value::as_bool) == Some(true)
                || block.get("isError").and_then(Value::as_bool) == Some(true))
    })
}

/// Extract `tool_use_id` from a `tool_result` row.
#[must_use]
pub fn extract_tool_use_id(row: &TranscriptRow) -> Option<String> {
    let message = row.fields().message.as_ref()?;
    let content = message.get("content")?;
    let blocks = content.as_array()?;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Idempotency Guard: `true` if `text` already contains an omission placeholder (`[pruned: see read_omitted_content`).
fn is_already_pruned(text: &str) -> bool {
    text.trim_start()
        .starts_with("[pruned: see read_omitted_content")
}

/// Multi-criteria pruning evaluation for a single transcript row using a [`PruningPolicy`].
///
/// Evaluates:
/// 0. Trailing turn protection (`turn_age < preserve_recent_turns`) & error output preservation (`is_error && preserve_error_outputs`).
/// 1. Idempotency guard (skips already pruned rows starting with `[pruned: see read_omitted_content`).
/// 2. Size thresholds (character & word counts from matching rule or policy defaults).
/// 3. Turn age decay (`turn_age > max_turn_age`).
/// 4. Unreferenced turn decay (`turn_age > unreferenced_turn_decay && !is_referenced`).
///
/// # Errors
///
/// Returns an error if [`OmissionCache::insert`] fails.
#[allow(clippy::too_many_arguments)]
pub fn prune_tool_row_with_policy(
    row: &TranscriptRow,
    cache: &OmissionCache,
    session_id: &str,
    policy: &PruningPolicy,
    turn_age: usize,
    is_referenced: bool,
    is_error: bool,
    resolved_tool_name: Option<&str>,
    dry_run: bool,
) -> Result<(PrunedRow, Option<PruneReason>)> {
    if !policy.enabled {
        return Ok((PrunedRow::Unchanged(row.clone()), None));
    }

    let Some((extracted_name, text)) = extract_tool_result(row) else {
        return Ok((PrunedRow::Unchanged(row.clone()), None));
    };

    // Idempotency Guard (Task 3.1.2)
    if is_already_pruned(&text) {
        return Ok((PrunedRow::Unchanged(row.clone()), None));
    }

    let tool_name = resolved_tool_name.unwrap_or(&extracted_name);
    let is_err = is_error || extract_is_error(row);

    // Rule 0: Skip if protected trailing turn or error output preservation
    let is_protected_turn = turn_age < policy.preserve_recent_turns;
    if is_protected_turn || (is_err && policy.preserve_error_outputs) {
        return Ok((PrunedRow::Unchanged(row.clone()), None));
    }

    let matching_rule = policy.find_matching_rule(tool_name);

    let char_len = text.chars().count();
    let word_count = text.split_whitespace().count();

    let limit_chars = matching_rule
        .and_then(|r| r.limit_chars)
        .unwrap_or(policy.default_limit_chars);

    let limit_words =
        matching_rule.map_or_else(|| Some(policy.default_limit_words), |r| r.limit_words);

    let exceeds_chars = char_len > limit_chars;
    let exceeds_words = limit_words.is_some_and(|w| word_count > w);

    let max_turn_age = matching_rule
        .and_then(|r| r.max_turn_age)
        .or(policy.max_turn_age);

    let exceeds_turn_age = max_turn_age.is_some_and(|max_age| turn_age > max_age);

    let unref_decay = matching_rule
        .and_then(|r| r.unreferenced_turn_decay)
        .or(policy.unreferenced_turn_decay);

    let exceeds_unref_decay =
        unref_decay.is_some_and(|decay_turns| turn_age > decay_turns && !is_referenced);

    let force_prune = matching_rule.is_some_and(|r| r.force_prune);

    let reason = if exceeds_chars || exceeds_words {
        Some(PruneReason::SizeThreshold)
    } else if exceeds_turn_age || force_prune {
        Some(PruneReason::TurnAge)
    } else if exceeds_unref_decay {
        Some(PruneReason::UnreferencedDecay)
    } else {
        None
    };

    let Some(prune_reason) = reason else {
        return Ok((PrunedRow::Unchanged(row.clone()), None));
    };

    let content_id = if dry_run {
        format!("simulated-{}", uuid::Uuid::new_v4().simple())
    } else {
        cache.insert(session_id, tool_name, &text)?
    };

    let placeholder = format!("[pruned: see read_omitted_content(session_id, \"{content_id}\")]");

    let mut fields = row.fields().clone();
    if let Some(message) = fields.message.as_mut() {
        if let Some(content) = message.get_mut("content") {
            match content {
                Value::String(s) => s.clone_from(&placeholder),
                Value::Array(blocks) => {
                    for block in blocks.iter_mut() {
                        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                            if let Some(inner) = block.get_mut("content") {
                                *inner = Value::String(placeholder.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let pruned_row = PrunedRow::Pruned {
        row: with_fields(row, fields),
        content_id,
        reason: prune_reason,
    };

    Ok((pruned_row, Some(prune_reason)))
}

/// Legacy single-row pruning threshold check against default policy (no turn protection).
///
/// # Errors
///
/// Returns an error if [`OmissionCache::insert`] fails.
pub fn prune_tool_row(
    row: &TranscriptRow,
    cache: &OmissionCache,
    session_id: &str,
) -> Result<PrunedRow> {
    let policy = PruningPolicy {
        preserve_recent_turns: 0,
        ..PruningPolicy::default()
    };
    let (pruned, _) = prune_tool_row_with_policy(
        row, cache, session_id, &policy, 0, false, false, None, false,
    )?;
    Ok(pruned)
}

/// Enforce cumulative tool context byte limits (`max_tool_context_bytes`) across unpruned tool output rows.
///
/// Uses LRU/LRR candidate ordering: candidates are sorted by tuple `(is_referenced, last_reference_turn_index, turn_index)` ascending,
/// evicting unreferenced older outputs first.
///
struct CapacityCandidate {
    row_idx: usize,
    text: String,
    tool_name: String,
    is_referenced: bool,
    last_ref_turn: usize,
    turn_index: usize,
}

/// Enforce cumulative tool context byte limits (`max_tool_context_bytes`) across unpruned tool output rows.
///
/// Uses LRU/LRR candidate ordering: candidates are sorted by tuple `(is_referenced, last_reference_turn_index, turn_index)` ascending,
/// evicting unreferenced older outputs first.
///
/// Precedence rule: trailing protected turns (`preserve_recent_turns`) CANNOT be evicted.
/// Emits `tracing::warn!` if protected trailing turns alone exceed `max_tool_context_bytes`.
///
/// # Errors
///
/// Returns an error if [`OmissionCache::insert`] fails.
#[allow(clippy::too_many_lines)]
pub fn prune_session_capacity(
    rows: &mut [TranscriptRow],
    turns: &[Turn],
    cache: &OmissionCache,
    session_id: &str,
    policy: &PruningPolicy,
    dry_run: bool,
    stats: &mut PruningStats,
) -> Result<()> {
    let Some(max_bytes) = policy.max_tool_context_bytes else {
        return Ok(());
    };

    let total_turns = turns.len();
    let row_turn_map = build_row_turn_indices(rows, turns);
    let tool_name_map = ToolNameMap::build(rows);
    let ref_map = ReferenceMap::build(rows, turns, &row_turn_map);

    let mut current_tool_bytes = 0usize;
    let mut protected_tool_bytes = 0usize;
    let mut candidates = Vec::new();

    for (idx, row) in rows.iter().enumerate() {
        let Some((tool_name, text)) = extract_tool_result_with_map(row, Some(&tool_name_map))
        else {
            continue;
        };
        if is_already_pruned(&text) {
            continue;
        }

        let text_bytes = text.len();
        current_tool_bytes += text_bytes;

        let turn_idx = row_turn_map.get(row.uuid()).copied().unwrap_or(0);
        let turn_age = relative_turn_age(turn_idx, total_turns);
        let is_protected = turn_age < policy.preserve_recent_turns;

        if is_protected {
            protected_tool_bytes += text_bytes;
        } else {
            let tool_use_id = extract_tool_use_id(row);
            let is_referenced = tool_use_id
                .as_deref()
                .is_some_and(|id| ref_map.is_referenced(id));
            let last_ref_turn = tool_use_id
                .as_deref()
                .and_then(|id| ref_map.last_reference_turn_index(id))
                .unwrap_or(0);

            candidates.push(CapacityCandidate {
                row_idx: idx,
                text,
                tool_name,
                is_referenced,
                last_ref_turn,
                turn_index: turn_idx,
            });
        }
    }

    if protected_tool_bytes > max_bytes {
        warn!(
            protected_bytes = protected_tool_bytes,
            max_bytes = max_bytes,
            "Protected trailing turns exceed max_tool_context_bytes capacity budget"
        );
    }

    if current_tool_bytes <= max_bytes {
        return Ok(());
    }

    // Sort candidates ascending by (is_referenced, last_ref_turn, turn_index)
    candidates.sort_by(|a, b| {
        a.is_referenced
            .cmp(&b.is_referenced)
            .then_with(|| a.last_ref_turn.cmp(&b.last_ref_turn))
            .then_with(|| a.turn_index.cmp(&b.turn_index))
    });

    for candidate in candidates {
        if current_tool_bytes <= max_bytes {
            break;
        }

        let content_id = if dry_run {
            format!("simulated-{}", uuid::Uuid::new_v4().simple())
        } else {
            cache.insert(session_id, &candidate.tool_name, &candidate.text)?
        };

        let placeholder =
            format!("[pruned: see read_omitted_content(session_id, \"{content_id}\")]");

        let row = &rows[candidate.row_idx];
        let mut fields = row.fields().clone();
        if let Some(message) = fields.message.as_mut() {
            if let Some(content) = message.get_mut("content") {
                match content {
                    Value::String(s) => s.clone_from(&placeholder),
                    Value::Array(blocks) => {
                        for block in blocks.iter_mut() {
                            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                                if let Some(inner) = block.get_mut("content") {
                                    *inner = Value::String(placeholder.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        rows[candidate.row_idx] = with_fields(row, fields);
        let freed = candidate.text.len();
        current_tool_bytes = current_tool_bytes.saturating_sub(freed);

        stats.rows_pruned += 1;
        stats.pruned_by_reason.capacity_lru += 1;
    }

    Ok(())
}

/// Executes a full multi-criteria and capacity LRU pruning pass over a transcript's rows.
///
/// # Errors
///
/// Returns an error if SQLite insertion fails during active mode.
pub fn prune_session_with_policy(
    rows: &[TranscriptRow],
    turns: &[Turn],
    cache: &OmissionCache,
    session_id: &str,
    policy: &PruningPolicy,
    dry_run: bool,
) -> Result<(Vec<TranscriptRow>, PruneExecutionReport, PruningStats)> {
    let total_turns = turns.len();
    let row_turn_map = build_row_turn_indices(rows, turns);
    let tool_name_map = ToolNameMap::build(rows);
    let ref_map = ReferenceMap::build(rows, turns, &row_turn_map);

    let mut out_rows = Vec::with_capacity(rows.len());
    let mut stats = PruningStats::default();

    for row in rows {
        stats.rows_checked += 1;

        if let Some((_, text)) = extract_tool_result_with_map(row, Some(&tool_name_map)) {
            stats.bytes_before += text.len();
        }

        let turn_idx = row_turn_map.get(row.uuid()).copied().unwrap_or(0);
        let turn_age = relative_turn_age(turn_idx, total_turns);

        let tool_use_id = extract_tool_use_id(row);
        let is_ref = tool_use_id
            .as_deref()
            .is_some_and(|id| ref_map.is_referenced(id));

        let tool_name_extracted = extract_tool_result_with_map(row, None);
        let tool_name = tool_use_id
            .as_deref()
            .and_then(|id| tool_name_map.get(id))
            .or_else(|| tool_name_extracted.as_ref().map(|(n, _)| n.as_str()));

        let is_err = extract_is_error(row);

        let (pruned_res, reason) = prune_tool_row_with_policy(
            row, cache, session_id, policy, turn_age, is_ref, is_err, tool_name, dry_run,
        )?;

        match pruned_res {
            PrunedRow::Unchanged(r) => out_rows.push(r),
            PrunedRow::Pruned { row: r, .. } => {
                out_rows.push(r);
                stats.rows_pruned += 1;
                if let Some(r_type) = reason {
                    match r_type {
                        PruneReason::SizeThreshold => stats.pruned_by_reason.size_threshold += 1,
                        PruneReason::TurnAge => stats.pruned_by_reason.turn_age += 1,
                        PruneReason::UnreferencedDecay => {
                            stats.pruned_by_reason.unreferenced_decay += 1;
                        }
                        PruneReason::CapacityLru => stats.pruned_by_reason.capacity_lru += 1,
                    }
                }
            }
        }
    }

    // Capacity LRU pass
    prune_session_capacity(
        &mut out_rows,
        turns,
        cache,
        session_id,
        policy,
        dry_run,
        &mut stats,
    )?;

    // Calculate bytes after
    for row in &out_rows {
        if let Some((_, text)) = extract_tool_result_with_map(row, Some(&tool_name_map)) {
            stats.bytes_after += text.len();
        }
    }

    stats.update_totals();

    let report = PruneExecutionReport {
        session_id: session_id.to_string(),
        rows_evaluated: stats.rows_checked,
        rows_pruned: stats.rows_pruned,
        bytes_freed: stats.bytes_freed(),
        estimated_tokens_saved: stats.estimated_tokens_saved,
        pruned_by_reason: stats.pruned_by_reason.clone(),
        dry_run,
    };

    Ok((out_rows, report, stats))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude_code_session::transcript::build_turns;
    use tempfile::TempDir;

    fn tool_result_row(tool_name: Option<&str>, content: &str) -> TranscriptRow {
        tool_result_row_full("t1", "a1", tool_name, "x", content, false)
    }

    fn tool_result_row_full(
        uuid: &str,
        parent_uuid: &str,
        tool_name: Option<&str>,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> TranscriptRow {
        let name_field = tool_name
            .map(|n| format!(r#","tool_name":"{n}""#))
            .unwrap_or_default();
        let err_field = if is_error { r#","is_error":true"# } else { "" };
        let line = format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":"{parent_uuid}","isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":{}{}{}}}]}}}}"#,
            serde_json::to_string(content).unwrap(),
            name_field,
            err_field,
        );
        serde_json::from_str(&line).unwrap()
    }

    fn user_row(uuid: &str, parent_uuid: Option<&str>, text: &str) -> TranscriptRow {
        let p_field = parent_uuid
            .map(|p| format!(r#","parentUuid":"{p}""#))
            .unwrap_or_default();
        let line = format!(
            r#"{{"type":"user","uuid":"{uuid}"{p_field},"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        );
        serde_json::from_str(&line).unwrap()
    }

    fn assistant_row(uuid: &str, parent_uuid: &str, text: &str) -> TranscriptRow {
        let line = format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent_uuid}","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        );
        serde_json::from_str(&line).unwrap()
    }

    fn assistant_row_with_tool_use(
        uuid: &str,
        parent_uuid: &str,
        tool_use_id: &str,
        tool_name: &str,
    ) -> TranscriptRow {
        let line = format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent_uuid}","isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{tool_use_id}","name":"{tool_name}","input":{{}}}}]}}}}"#
        );
        serde_json::from_str(&line).unwrap()
    }

    fn open_cache() -> (TempDir, OmissionCache) {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("omission-cache.sqlite");
        let cache = OmissionCache::open(&cache_path).unwrap();
        (dir, cache)
    }

    #[test]
    fn prune_tool_row_should_return_pruned_when_content_exceeds_default_char_limit() {
        let (_f, cache) = open_cache();
        let long_content = "x".repeat(DEFAULT_LIMIT_CHARS + 1);
        let row = tool_result_row(Some("SomeTool"), &long_content);

        let result = prune_tool_row(&row, &cache, "session-1").unwrap();

        let PrunedRow::Pruned {
            row, content_id, ..
        } = result
        else {
            panic!("expected Pruned, got Unchanged");
        };
        let cached = cache.get("session-1", &content_id).unwrap();
        assert_eq!(
            cached,
            Some(long_content),
            "cached content must round-trip byte-for-byte"
        );

        let placeholder = row
            .fields()
            .message
            .as_ref()
            .unwrap()
            .pointer("/content/0/content")
            .and_then(Value::as_str)
            .unwrap();
        assert!(placeholder.contains(&content_id));
    }

    #[test]
    fn prune_tool_row_should_return_unchanged_when_content_is_under_limit() {
        let (_f, cache) = open_cache();
        let short_content = "short output";
        let row = tool_result_row(Some("SomeTool"), short_content);

        let result = prune_tool_row(&row, &cache, "session-1").unwrap();

        let PrunedRow::Unchanged(unchanged) = result else {
            panic!("expected Unchanged, got Pruned");
        };
        assert_eq!(unchanged, row, "row must be untouched below threshold");
    }

    /// UT-EVAL-001: Single-row character (`default_limit_chars`) and word (`default_limit_words`) threshold pruning evaluation.
    #[test]
    fn ut_eval_001_single_row_char_and_word_threshold_evaluation() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        // Exceeds chars
        let long_chars = "x".repeat(DEFAULT_LIMIT_CHARS + 1);
        let row_chars = tool_result_row(Some("SomeTool"), &long_chars);
        let (res_chars, reason_chars) = prune_tool_row_with_policy(
            &row_chars,
            &cache,
            "session-1",
            &policy,
            0,
            false,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_chars, PrunedRow::Pruned { .. }));
        assert_eq!(reason_chars, Some(PruneReason::SizeThreshold));

        // Exceeds words
        let words = vec!["word"; DEFAULT_LIMIT_WORDS + 1].join(" ");
        let row_words = tool_result_row(Some("SomeTool"), &words);
        let (res_words, reason_words) = prune_tool_row_with_policy(
            &row_words,
            &cache,
            "session-1",
            &policy,
            0,
            false,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_words, PrunedRow::Pruned { .. }));
        assert_eq!(reason_words, Some(PruneReason::SizeThreshold));
    }

    /// UT-EVAL-002: Turn age decay threshold evaluation (`max_turn_age`) against relative turn age.
    #[test]
    fn ut_eval_002_turn_age_decay_threshold_evaluation() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            max_turn_age: Some(5),
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        let short_content = "short output";
        let row = tool_result_row(Some("SomeTool"), short_content);

        // Turn age 4 <= max_turn_age 5 -> Unchanged
        let (res_young, reason_young) = prune_tool_row_with_policy(
            &row,
            &cache,
            "session-1",
            &policy,
            4,
            true,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_young, PrunedRow::Unchanged(_)));
        assert_eq!(reason_young, None);

        // Turn age 6 > max_turn_age 5 -> Pruned by TurnAge
        let (res_old, reason_old) = prune_tool_row_with_policy(
            &row,
            &cache,
            "session-1",
            &policy,
            6,
            true,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_old, PrunedRow::Pruned { .. }));
        assert_eq!(reason_old, Some(PruneReason::TurnAge));
    }

    /// UT-EVAL-003: Unreferenced turn decay evaluation (`unreferenced_turn_decay`) asserting unreferenced outputs are evicted after decay window.
    #[test]
    fn ut_eval_003_unreferenced_turn_decay_evaluation() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            unreferenced_turn_decay: Some(3),
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        let short_content = "short output";
        let row = tool_result_row(Some("SomeTool"), short_content);

        // Referenced & age 4 > 3 -> Unchanged (since referenced)
        let (res_ref, reason_ref) = prune_tool_row_with_policy(
            &row,
            &cache,
            "session-1",
            &policy,
            4,
            true,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_ref, PrunedRow::Unchanged(_)));
        assert_eq!(reason_ref, None);

        // Unreferenced & age 4 > 3 -> Pruned by UnreferencedDecay
        let (res_unref, reason_unref) = prune_tool_row_with_policy(
            &row,
            &cache,
            "session-1",
            &policy,
            4,
            false,
            false,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_unref, PrunedRow::Pruned { .. }));
        assert_eq!(reason_unref, Some(PruneReason::UnreferencedDecay));
    }

    /// UT-EVAL-004: Diagnostic error output preservation when `is_error == true` and `preserve_error_outputs == true`.
    #[test]
    fn ut_eval_004_error_output_preservation() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            preserve_error_outputs: true,
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        let long_error = "ERROR: ".to_string() + &"x".repeat(DEFAULT_LIMIT_CHARS + 10);
        let err_row = tool_result_row_full("t1", "a1", Some("Bash"), "x1", &long_error, true);

        // Error output should stay Unchanged even if oversized
        let (res_err, reason_err) = prune_tool_row_with_policy(
            &err_row,
            &cache,
            "session-1",
            &policy,
            10,
            false,
            true,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(res_err, PrunedRow::Unchanged(_)));
        assert_eq!(reason_err, None);
    }

    /// UT-IDEM-001: Idempotency guard verifying strings starting with `"[pruned: see read_omitted_content"` return `PrunedRow::Unchanged`.
    #[test]
    fn ut_idem_001_idempotency_guard() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy::default();

        let placeholder = "[pruned: see read_omitted_content(session-1, \"omitted-001\")]";
        let pruned_row = tool_result_row(Some("SomeTool"), placeholder);

        let (res, reason) = prune_tool_row_with_policy(
            &pruned_row,
            &cache,
            "session-1",
            &policy,
            10,
            false,
            false,
            None,
            false,
        )
        .unwrap();

        let PrunedRow::Unchanged(unchanged_row) = res else {
            panic!("expected Unchanged from Idempotency Guard");
        };
        assert_eq!(unchanged_row, pruned_row);
        assert_eq!(reason, None);
    }

    /// UT-CAP-001: Transcript unpruned tool context cumulative byte calculation against `max_tool_context_bytes`.
    #[test]
    fn ut_cap_001_capacity_limit_triggers_eviction() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            max_tool_context_bytes: Some(100),
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        let u1 = user_row("u1", None, "User request 1");
        let a1 = assistant_row_with_tool_use("a1", "u1", "call_1", "SomeTool");
        let r1 = tool_result_row_full(
            "r1",
            "a1",
            Some("SomeTool"),
            "call_1",
            &"a".repeat(80),
            false,
        );

        let u2 = user_row("u2", Some("r1"), "User request 2");
        let a2 = assistant_row_with_tool_use("a2", "u2", "call_2", "SomeTool");
        let r2 = tool_result_row_full(
            "r2",
            "a2",
            Some("SomeTool"),
            "call_2",
            &"b".repeat(80),
            false,
        );

        let rows = vec![u1, a1, r1, a2, r2, u2];
        let turns = build_turns(&rows).unwrap();

        let (_out_rows, _report, stats) =
            prune_session_with_policy(&rows, &turns, &cache, "session-1", &policy, false).unwrap();

        assert!(stats.pruned_by_reason.capacity_lru >= 1);
        assert!(stats.rows_pruned >= 1);
    }

    /// UT-CAP-002: Candidate sorting logic for LRU eviction ordered by `(is_referenced, last_reference_turn_index, turn_index)` ascending.
    #[test]
    fn ut_cap_002_candidate_sorting_lru_eviction() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            max_tool_context_bytes: Some(90),
            preserve_recent_turns: 0,
            ..PruningPolicy::default()
        };

        // Turn 0: r1 (unreferenced, 60 bytes)
        let u1 = user_row("u1", None, "User 1");
        let a1 = assistant_row_with_tool_use("a1", "u1", "c1", "SomeTool");
        let r1 = tool_result_row_full("r1", "a1", Some("SomeTool"), "c1", &"1".repeat(60), false);

        // Turn 1: r2 (referenced, 60 bytes)
        let u2 = user_row("u2", Some("r1"), "User 2");
        let a2 = assistant_row_with_tool_use("a2", "u2", "c2", "SomeTool");
        let r2 = tool_result_row_full("r2", "a2", Some("SomeTool"), "c2", &"2".repeat(60), false);

        // Turn 2: assistant cites c2
        let u3 = user_row("u3", Some("r2"), "User 3");
        let a3 = assistant_row("a3", "u3", "Here is the result from c2: Output");

        let rows = vec![u1, a1, r1, u2, a2, r2, u3, a3];
        let turns = build_turns(&rows).unwrap();

        let (out_rows, _report, stats) =
            prune_session_with_policy(&rows, &turns, &cache, "session-1", &policy, false).unwrap();

        assert_eq!(stats.pruned_by_reason.capacity_lru, 1);

        // r1 (unreferenced) must be evicted first
        let r1_out = out_rows.iter().find(|r| r.uuid() == "r1").unwrap();
        let text1 = extract_tool_result(r1_out).unwrap().1;
        assert!(text1.contains("[pruned: see read_omitted_content"));

        // r2 (referenced) remains unpruned
        let r2_out = out_rows.iter().find(|r| r.uuid() == "r2").unwrap();
        let text2 = extract_tool_result(r2_out).unwrap().1;
        assert!(!text2.contains("[pruned: see read_omitted_content"));
    }

    /// UT-CAP-003: Trailing turn protection precedence over capacity caps emitting `tracing::warn!`.
    #[test]
    fn ut_cap_003_protected_turns_precedence_over_capacity() {
        let (_f, cache) = open_cache();
        let policy = PruningPolicy {
            max_tool_context_bytes: Some(50),
            preserve_recent_turns: 2, // Protects last 2 turns
            ..PruningPolicy::default()
        };

        // Turn 0 (protected): 100 bytes tool output
        let u1 = user_row("u1", None, "User 1");
        let a1 = assistant_row_with_tool_use("a1", "u1", "c1", "SomeTool");
        let r1 = tool_result_row_full("r1", "a1", Some("SomeTool"), "c1", &"x".repeat(100), false);

        let rows = vec![u1, a1, r1];
        let turns = build_turns(&rows).unwrap();

        let (out_rows, _report, stats) =
            prune_session_with_policy(&rows, &turns, &cache, "session-1", &policy, false).unwrap();

        // Protected turn output (100 bytes > 50 byte max) must NOT be evicted
        assert_eq!(stats.pruned_by_reason.capacity_lru, 0);
        let r1_out = out_rows.iter().find(|r| r.uuid() == "r1").unwrap();
        let text1 = extract_tool_result(r1_out).unwrap().1;
        assert_eq!(text1, "x".repeat(100));
    }
}
