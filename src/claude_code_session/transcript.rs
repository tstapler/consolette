//! JSONL transcript parsing and turn/chain reconstruction for Claude Code
//! session files (`~/.claude/projects/**/*.jsonl`).
//!
//! Unlike `crate::learn::transcript`, this module never drops a row's
//! unrecognized fields — every field the destination writer (Phase 4)
//! doesn't explicitly model must round-trip byte-faithfully, since it will
//! be re-serialized into a rewritten session file. See
//! `project_plans/compaction-hook/decisions/ADR-008-claude-code-session-module-placement.md`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// TranscriptRow
// ---------------------------------------------------------------------------

/// Fields shared by every transcript row shape (`user`, `assistant`,
/// `system`, and anything Claude Code adds later that this module doesn't
/// yet understand).
///
/// `extra` collects every JSON key not named explicitly below — including
/// `type` itself, which is *not* given a dedicated struct field here (see
/// [`TranscriptRow`]'s manual `Deserialize`/`Serialize` impls) — so
/// re-serializing a row never drops data from an evolving, undocumented
/// schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFields {
    pub uuid: String,
    #[serde(rename = "parentUuid", default)]
    pub parent_uuid: Option<String>,
    #[serde(rename = "isSidechain", default)]
    pub is_sidechain: bool,
    #[serde(rename = "isMeta", default)]
    pub is_meta: bool,
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A single transcript row, tagged by its JSON `type` field.
///
/// This is conceptually `#[serde(tag = "type")]` with `User`/`Assistant`/
/// `System`/`#[serde(other)] Unknown` variants, per
/// `project_plans/compaction-hook/implementation/plan.md`'s Story 1.1.1 —
/// but serde's derive does not support that literally: `#[serde(other)]`
/// on an internally tagged enum **requires the catch-all to be a unit
/// variant** (verified against serde 1.0.229 — a data-carrying `Unknown`
/// variant fails to compile with "`#[serde(other)]` must be on a unit
/// variant"). Since this module's whole point is round-tripping fields it
/// doesn't understand, an `Unknown` that drops its `uuid`/`parentUuid`
/// would defeat that purpose. `TranscriptRow` therefore implements
/// `Deserialize`/`Serialize` by hand below: peek the `type` field, dispatch
/// to the matching variant, and let every variant carry the same
/// [`RowFields`] (whose `extra` flatten already retains the original
/// `type` string verbatim, so serialization doesn't need to re-add a tag).
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptRow {
    User(RowFields),
    Assistant(RowFields),
    System(RowFields),
    Unknown(RowFields),
}

impl TranscriptRow {
    /// The fields shared by all variants.
    #[must_use]
    pub fn fields(&self) -> &RowFields {
        match self {
            TranscriptRow::User(f)
            | TranscriptRow::Assistant(f)
            | TranscriptRow::System(f)
            | TranscriptRow::Unknown(f) => f,
        }
    }

    #[must_use]
    pub fn uuid(&self) -> &str {
        &self.fields().uuid
    }

    #[must_use]
    pub fn parent_uuid(&self) -> Option<&str> {
        self.fields().parent_uuid.as_deref()
    }

    #[must_use]
    pub fn is_sidechain(&self) -> bool {
        self.fields().is_sidechain
    }

    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, TranscriptRow::User(_))
    }
}

impl<'de> Deserialize<'de> for TranscriptRow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let row_type = value.get("type").and_then(Value::as_str).map(str::to_owned);
        let fields = RowFields::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(match row_type.as_deref() {
            Some("user") => TranscriptRow::User(fields),
            Some("assistant") => TranscriptRow::Assistant(fields),
            Some("system") => TranscriptRow::System(fields),
            _ => TranscriptRow::Unknown(fields),
        })
    }
}

impl Serialize for TranscriptRow {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // `fields().extra` already contains the original `type` string
        // (it was never consumed by a dedicated struct field), so
        // serializing `RowFields` directly reproduces it without this impl
        // needing to re-insert a tag.
        self.fields().serialize(serializer)
    }
}

// ---------------------------------------------------------------------------
// parse_session_file
// ---------------------------------------------------------------------------

/// Parse a Claude Code session JSONL file into typed rows.
///
/// Streams the file line-by-line via [`BufReader::lines`] rather than
/// reading the whole file into memory first (matching
/// `src/bin/cmdcrush/main.rs`'s pattern, not `src/learn/transcript.rs`'s
/// whole-file-read pattern), so memory use stays bounded by one line at a
/// time regardless of file size.
///
/// A line that fails to parse as JSON (or fails `TranscriptRow`'s shape) is
/// logged via `tracing::warn!` with its 1-indexed line number and skipped —
/// this mirrors `src/learn/transcript.rs`'s defensive-parse convention,
/// since the Claude Code transcript schema has no official spec and can
/// change without notice. Blank lines are skipped silently.
///
/// # Errors
///
/// Returns an error if `path` cannot be opened, or if reading a line from
/// the open file fails (e.g. invalid UTF-8, an I/O error mid-read). Neither
/// case includes a bad *parse* of a well-formed line — that's the skip path
/// described above.
pub fn parse_session_file(path: &Path) -> Result<Vec<TranscriptRow>> {
    let file = File::open(path)
        .map_err(|error| anyhow!("failed to open session file {}: {error}", path.display()))?;
    let reader = BufReader::new(file);
    let mut rows = Vec::new();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            anyhow!(
                "failed to read line {} of {}: {error}",
                line_no + 1,
                path.display()
            )
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<TranscriptRow>(trimmed) {
            Ok(row) => rows.push(row),
            Err(error) => {
                warn!(
                    line = line_no + 1,
                    %error,
                    "skipping unparseable transcript row"
                );
            }
        }
    }

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Turn / build_turns
// ---------------------------------------------------------------------------

/// A logical turn: one genuine user message, plus the assistant/tool rows
/// that followed it before the next genuine user message.
///
/// "Genuine" excludes Claude Code's tool-result rows, which are `type:
/// "user"` at the JSON level but carry only `tool_result` content blocks —
/// those are classified into `tool_rows`, not treated as a new turn
/// boundary (see [`build_turns`]'s doc comment for the full classification
/// rule).
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub user_row: TranscriptRow,
    pub assistant_rows: Vec<TranscriptRow>,
    pub tool_rows: Vec<TranscriptRow>,
}

/// Group parsed rows into turns by following `parent_uuid` links, with
/// cycle detection.
///
/// Algorithm (mirrors magic-compact's `buildActiveChain` +
/// `recoverParallelToolRows`, `research/features.md`, citing
/// `transcript.ts:114-258`):
///
/// 1. Build a `uuid -> row` map from **non-sidechain rows only** — a
///    sidechain row is never inserted, so a `parent_uuid` pointing at one is
///    indistinguishable from a `parent_uuid` pointing at any other missing
///    `uuid`; both hit the dangling-reference branch below.
/// 2. Walk `parent_uuid` links backward from the last non-sidechain row,
///    building the "active chain." A `uuid` revisited during this walk is a
///    cycle: returns `Err` rather than looping forever. A `parent_uuid` not
///    found in the map (dangling — sidechain-excluded or otherwise missing)
///    is tolerated: the walk stops there and that row becomes the chain's
///    root, logged via `tracing::debug!`, never an error.
/// 3. Recover parallel tool-call rows: any non-sidechain row *not* on the
///    active chain whose `parent_uuid` *is* on the chain (siblings sharing
///    one assistant parent) is folded in next to that parent instead of
///    being dropped.
/// 4. Fold the ordered chain into `Turn`s: a new `Turn` starts at each
///    genuine user row; every other row (assistant/system/unknown, and
///    `user`-typed tool-result-carrier rows) attaches to the current turn's
///    `assistant_rows` or `tool_rows` respectively, until the next genuine
///    user row.
///
/// A row is classified as a tool-result carrier (and thus grouped into
/// `tool_rows` rather than starting a new turn) when it is `type: "user"`
/// and its `message.content` is a non-empty array whose blocks are all
/// `type: "tool_result"` — the shape Claude Code uses to report tool
/// output back to the model. This classification rule isn't specified
/// byte-for-byte in the design docs; it's the most direct reading of
/// "tool rows sharing an assistant parent get grouped into `tool_rows`."
///
/// # Errors
///
/// Returns an error if the active parent chain contains a cycle.
pub fn build_turns(rows: &[TranscriptRow]) -> Result<Vec<Turn>> {
    let mut by_uuid: HashMap<&str, &TranscriptRow> = HashMap::new();
    for row in rows {
        if !row.is_sidechain() {
            by_uuid.insert(row.uuid(), row);
        }
    }

    let Some(last) = rows.iter().rev().find(|row| !row.is_sidechain()) else {
        return Ok(Vec::new());
    };

    // Step 1/2: walk the active chain backward from `last`.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut chain_rev: Vec<&str> = Vec::new();
    let mut current = last.uuid();
    loop {
        if !visited.insert(current) {
            return Err(anyhow!("parent-chain cycle at {current}"));
        }
        chain_rev.push(current);

        let Some(row) = by_uuid.get(current) else {
            // `current` always originates either from `last` (guaranteed
            // non-sidechain, so present) or from a parent already
            // confirmed present in the map below — this branch is
            // unreachable but handled defensively rather than panicking.
            break;
        };

        match row.parent_uuid() {
            None => break,
            Some(parent) => {
                if by_uuid.contains_key(parent) {
                    current = parent;
                } else {
                    debug!(
                        uuid = current,
                        missing_parent = parent,
                        "parent row excluded as sidechain or otherwise missing; starting new turn"
                    );
                    break;
                }
            }
        }
    }
    chain_rev.reverse();
    let chain_order = chain_rev;
    let chain_uuids: HashSet<&str> = chain_order.iter().copied().collect();

    // Step 3: recover parallel tool-call rows sharing a parent on the chain.
    let mut siblings_of: HashMap<&str, Vec<&TranscriptRow>> = HashMap::new();
    for row in rows {
        if row.is_sidechain() || chain_uuids.contains(row.uuid()) {
            continue;
        }
        if let Some(parent) = row.parent_uuid() {
            if chain_uuids.contains(parent) {
                siblings_of.entry(parent).or_default().push(row);
            }
        }
    }

    // Step 4: fold the ordered chain (plus recovered siblings) into Turns.
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_turn: Option<Turn> = None;
    for uuid in chain_order {
        let Some(row) = by_uuid.get(uuid).copied() else {
            continue;
        };
        push_row_into_turns(row.clone(), &mut turns, &mut current_turn);
        if let Some(extra_rows) = siblings_of.remove(uuid) {
            for extra in extra_rows {
                push_row_into_turns(extra.clone(), &mut turns, &mut current_turn);
            }
        }
    }
    if let Some(turn) = current_turn.take() {
        turns.push(turn);
    }

    Ok(turns)
}

/// `true` when `row` is `type: "user"` and carries only `tool_result`
/// content blocks — Claude Code's shape for reporting tool output back to
/// the model, as opposed to a genuine new user message.
fn is_tool_result_carrier(row: &TranscriptRow) -> bool {
    if !row.is_user() {
        return false;
    }
    let Some(message) = &row.fields().message else {
        return false;
    };
    let Some(content) = message.get("content") else {
        return false;
    };
    match content {
        Value::Array(blocks) => {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        }
        _ => false,
    }
}

fn push_row_into_turns(row: TranscriptRow, turns: &mut Vec<Turn>, current_turn: &mut Option<Turn>) {
    if row.is_user() && !is_tool_result_carrier(&row) {
        if let Some(finished) = current_turn.take() {
            turns.push(finished);
        }
        *current_turn = Some(Turn {
            user_row: row,
            assistant_rows: Vec::new(),
            tool_rows: Vec::new(),
        });
        return;
    }

    let Some(turn) = current_turn.as_mut() else {
        // A row before any genuine user row has started a turn (e.g. the
        // transcript opens with a system/assistant row, or a dangling
        // parent rooted a chain at a non-user row). There's no turn to
        // attach it to, so it's dropped from the reconstruction — rare in
        // real transcripts and flagged in plan.md as an accepted gap.
        debug!(
            uuid = row.uuid(),
            "row precedes any user turn; dropping from build_turns output"
        );
        return;
    };

    if is_tool_result_carrier(&row) {
        turn.tool_rows.push(row);
    } else {
        turn.assistant_rows.push(row);
    }
}

// ---------------------------------------------------------------------------
// ChainCoverage
// ---------------------------------------------------------------------------

/// How much of a transcript's genuine user/assistant message history
/// `build_turns` actually reconstructed onto the active chain.
///
/// A transcript with multiple disconnected conversation roots (e.g. from
/// repeated `--resume`/`--clear` cycles in a long-lived project directory)
/// has real message history before the first broken `parent_uuid` link that
/// `build_turns` never reaches — see its doc comment's "dangling parent is
/// tolerated" step. Low coverage means callers reasoning about "the whole
/// transcript" from `turns` alone (e.g. `compare_compaction_cost`) are
/// actually only seeing its most recent thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainCoverage {
    pub chain_messages: usize,
    pub total_messages: usize,
}

impl ChainCoverage {
    /// Fraction of `total_messages` present in `chain_messages`, in `[0,
    /// 1]`. `1.0` when there are no messages at all (nothing was excluded).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(&self) -> f64 {
        if self.total_messages == 0 {
            1.0
        } else {
            self.chain_messages as f64 / self.total_messages as f64
        }
    }
}

/// Computes [`ChainCoverage`] for a `(rows, turns)` pair produced by
/// [`parse_session_file`] and [`build_turns`] respectively.
///
/// Counts "genuine" user/assistant messages only — matching `build_turns`'s
/// own turn-boundary rule, tool-result-carrier rows and non-message
/// (system/unknown) rows are excluded from both sides of the ratio.
#[must_use]
pub fn chain_coverage(rows: &[TranscriptRow], turns: &[Turn]) -> ChainCoverage {
    let is_genuine_message = |row: &TranscriptRow| {
        matches!(row, TranscriptRow::User(_) | TranscriptRow::Assistant(_))
            && !is_tool_result_carrier(row)
    };

    let total_messages = rows
        .iter()
        .filter(|row| !row.is_sidechain() && is_genuine_message(row))
        .count();

    // Each turn's `user_row` is always genuine by construction
    // (`push_row_into_turns` only opens a turn on a genuine user row); only
    // `assistant_rows` needs filtering, since it can also hold
    // system/unknown rows riding along on the same chain.
    let chain_messages: usize = turns
        .iter()
        .map(|turn| {
            1 + turn
                .assistant_rows
                .iter()
                .filter(|row| is_genuine_message(row))
                .count()
        })
        .sum();

    ChainCoverage {
        chain_messages,
        total_messages,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn parent_uuid_json(parent: Option<&str>) -> String {
        parent.map_or_else(|| "null".to_string(), |p| format!("\"{p}\""))
    }

    fn user_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"message":{{"role":"user","content":"{text}"}}}}"#,
            parent_uuid_json(parent),
        )
    }

    fn assistant_row(uuid: &str, parent: Option<&str>, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{},"isSidechain":false,"isMeta":false,"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#,
            parent_uuid_json(parent),
        )
    }

    fn tool_result_row(uuid: &str, parent: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":"{parent}","isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"x","content":"ok"}}]}}}}"#
        )
    }

    // -- parse_session_file --

    #[test]
    fn parse_session_file_should_preserve_unknown_fields_when_row_has_extra_data() {
        let f = write_temp_jsonl(&[
            r#"{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"isMeta":false,"customField":"keep-me","message":{"role":"user","content":"hi"}}"#,
        ]);
        let rows = parse_session_file(f.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uuid(), "u1");
        assert_eq!(rows[0].parent_uuid(), None);
        assert!(matches!(rows[0], TranscriptRow::User(_)));
        assert_eq!(
            rows[0]
                .fields()
                .extra
                .get("customField")
                .and_then(Value::as_str),
            Some("keep-me")
        );
        assert_eq!(
            rows[0].fields().extra.get("type").and_then(Value::as_str),
            Some("user")
        );
    }

    #[test]
    fn parse_session_file_should_skip_and_warn_when_line_is_unparseable_json() {
        let f = write_temp_jsonl(&["not json at all {{{", &user_row("ok", None, "valid")]);
        let rows = parse_session_file(f.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uuid(), "ok");
    }

    #[test]
    fn parse_session_file_should_stream_large_fixture_without_loading_whole_file() {
        // Streams via BufReader::lines() (see parse_session_file's doc
        // comment); this test exercises that code path end-to-end against
        // a real multi-MB file on disk rather than asserting on internal
        // memory bounds directly.
        let mut f = NamedTempFile::new().unwrap();
        let row_count = 20_000;
        for i in 0..row_count {
            writeln!(f, "{}", user_row(&format!("u{i}"), None, "hello")).unwrap();
        }
        let metadata = std::fs::metadata(f.path()).unwrap();
        assert!(
            metadata.len() > 1_000_000,
            "fixture should be multi-MB to meaningfully exercise streaming"
        );

        let rows = parse_session_file(f.path()).unwrap();
        assert_eq!(rows.len(), row_count);
    }

    // -- build_turns --

    #[test]
    fn build_turns_should_reconstruct_two_turns_when_fixture_is_a_normal_conversation() {
        let f = write_temp_jsonl(&[
            &user_row("u1", None, "hello"),
            &assistant_row("a1", Some("u1"), "hi"),
            &user_row("u2", Some("a1"), "how are you"),
            &assistant_row("a2", Some("u2"), "good"),
        ]);
        let rows = parse_session_file(f.path()).unwrap();
        let turns = build_turns(&rows).unwrap();

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user_row.uuid(), "u1");
        assert_eq!(turns[0].assistant_rows.len(), 1);
        assert_eq!(turns[0].assistant_rows[0].uuid(), "a1");
        assert_eq!(turns[1].user_row.uuid(), "u2");
        assert_eq!(turns[1].assistant_rows[0].uuid(), "a2");
    }

    #[test]
    fn build_turns_should_group_parallel_tool_rows_into_one_turn_when_sharing_assistant_parent() {
        let rows_json = [
            user_row("u1", None, "do something"),
            assistant_row("a1", Some("u1"), "calling tools"),
            tool_result_row("t1", "a1"),
            tool_result_row("t2", "a1"),
            assistant_row("a2", Some("t2"), "done"),
        ];
        let rows: Vec<TranscriptRow> = rows_json
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let turns = build_turns(&rows).unwrap();
        assert_eq!(turns.len(), 1);
        let tool_uuids: HashSet<&str> =
            turns[0].tool_rows.iter().map(TranscriptRow::uuid).collect();
        assert_eq!(tool_uuids, HashSet::from(["t1", "t2"]));
        let assistant_uuids: HashSet<&str> = turns[0]
            .assistant_rows
            .iter()
            .map(TranscriptRow::uuid)
            .collect();
        assert_eq!(assistant_uuids, HashSet::from(["a1", "a2"]));
    }

    #[test]
    fn build_turns_should_return_err_when_parent_chain_cycle_detected() {
        let rows_json = [user_row("a", Some("b"), "x"), user_row("b", Some("a"), "y")];
        let rows: Vec<TranscriptRow> = rows_json
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let result = build_turns(&rows);
        assert!(result.is_err());
    }

    #[test]
    fn build_turns_should_start_new_chain_when_parent_uuid_points_at_excluded_sidechain_row() {
        let sidechain = r#"{"type":"user","uuid":"sc1","parentUuid":null,"isSidechain":true,"isMeta":false,"message":{"role":"user","content":"subagent"}}"#.to_string();
        let rows_json = [
            sidechain,
            user_row("a1", Some("sc1"), "new turn start"),
            assistant_row("b1", Some("a1"), "response"),
        ];
        let rows: Vec<TranscriptRow> = rows_json
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let turns = build_turns(&rows).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_row.uuid(), "a1");
        assert_eq!(turns[0].assistant_rows[0].uuid(), "b1");
    }

    // -- chain_coverage --

    #[test]
    fn chain_coverage_should_be_full_when_transcript_has_a_single_connected_chain() {
        let f = write_temp_jsonl(&[
            &user_row("u1", None, "hello"),
            &assistant_row("a1", Some("u1"), "hi"),
        ]);
        let rows = parse_session_file(f.path()).unwrap();
        let turns = build_turns(&rows).unwrap();

        let coverage = chain_coverage(&rows, &turns);
        assert_eq!(coverage.chain_messages, 2);
        assert_eq!(coverage.total_messages, 2);
        assert!((coverage.ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn chain_coverage_should_exclude_earlier_disconnected_conversation_root() {
        // Two entirely separate conversations in one file (e.g. from a
        // `--resume`/`--clear` cycle): u1/a1 shares no parent link with
        // u2/a2, so build_turns's active-chain walk starting from the last
        // row (a2) never reaches u1/a1.
        let f = write_temp_jsonl(&[
            &user_row("u1", None, "first conversation"),
            &assistant_row("a1", Some("u1"), "first reply"),
            &user_row("u2", None, "second conversation"),
            &assistant_row("a2", Some("u2"), "second reply"),
        ]);
        let rows = parse_session_file(f.path()).unwrap();
        let turns = build_turns(&rows).unwrap();

        let coverage = chain_coverage(&rows, &turns);
        assert_eq!(coverage.total_messages, 4);
        assert_eq!(coverage.chain_messages, 2);
        assert!((coverage.ratio() - 0.5).abs() < f64::EPSILON);
    }
}
