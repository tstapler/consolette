//! Per-tool-name pruning of bulky, completed tool I/O into
//! `src/claude_code_session/omission_cache.rs`, replacing the row's content
//! with a placeholder that embeds the resulting `content_id`.
//!
//! This is a **self-contained binary omit-over-threshold check on raw,
//! uncompressed content** — it deliberately never calls into
//! `crate::compression::*`. That module's fallback chain
//! (`diff_compactor`/`text_compressor`/`line_truncate`) exists for
//! consolette's separate live-proxy compaction use case and implements a
//! *partial* inline-compression model; magic-compact's own `prune.ts`
//! (`research/features.md` §4) never runs a compression pass before
//! deciding to omit — it's a binary "keep verbatim" vs. "omit entirely,
//! cache verbatim" decision. Reusing `crate::compression`'s primitives here
//! would silently substitute a different semantic model for the one this
//! plan explicitly commits to (see
//! `project_plans/compaction-hook/implementation/plan.md`'s Story 2.1.1
//! algorithm note), so `PrunedRow` stays a clean two-variant enum with no
//! third "compressed-but-inline" state.

use crate::claude_code_session::omission_cache::OmissionCache;
use crate::claude_code_session::transcript::{RowFields, TranscriptRow};
use anyhow::Result;
use serde_json::Value;

/// Default char threshold for tools with no more specific override.
pub const DEFAULT_LIMIT_CHARS: usize = 1024;
/// Default word threshold for tools with no more specific override.
pub const DEFAULT_LIMIT_WORDS: usize = 128;
/// Char threshold for the "agent output" tool class (`Agent`, `TaskOutput`
/// per `research/features.md` §4, citing `prune.ts:190`).
pub const AGENT_OUTPUT_LIMIT_CHARS: usize = 4096;
/// Word threshold for the "agent output" tool class.
pub const AGENT_OUTPUT_LIMIT_WORDS: usize = 512;
/// Flat char cutoff for `Bash` tool output — a length-only check, not a
/// word/char OR like the other classes (`research/features.md` §4, citing
/// `prune.ts:163-166`, which applies this flat rule to `Bash` *input*;
/// this module applies the same flat cutoff to `Bash` *output* pruning,
/// since Story 2.1.1's scope is `tool_result` content only).
pub const BASH_LIMIT_CHARS: usize = 1024;

/// Tool names whose output is measured against `AGENT_OUTPUT_LIMIT_*`
/// instead of `DEFAULT_LIMIT_*`, per `research/features.md` §4 (citing
/// `prune.ts:190`: "`Agent` / `TaskOutput`: use `AGENT_OUTPUT_LIMIT`").
const AGENT_OUTPUT_TOOL_NAMES: &[&str] = &["Agent", "TaskOutput"];

/// The outcome of [`prune_tool_row`]: either the row is returned untouched,
/// or its content was cached and replaced with a placeholder.
///
/// Exactly two variants, matching this module's binary (never partially
/// compressed) pruning model — see the module doc comment.
#[derive(Debug, Clone, PartialEq)]
pub enum PrunedRow {
    Unchanged(TranscriptRow),
    Pruned {
        row: TranscriptRow,
        content_id: String,
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

/// Flatten a `tool_result` block's nested `content` field (a plain string,
/// or a list of `{"type": "text", "text": ...}` blocks) into plain text.
///
/// Mirrors `src/bin/cmdcrush/main.rs::tool_result_text` exactly (that
/// helper is private to the `cmdcrush` binary, so it's duplicated here
/// rather than shared across crate boundaries).
fn tool_result_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let blocks = content.as_array()?;
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Extract `(tool_name, content_text)` from a `tool_result`-carrier row's
/// `message` JSON, or `None` if `row` doesn't carry any `tool_result`
/// content.
///
/// `message.content` is handled as either a bare string (defensive
/// fallback — real Claude Code tool-result carrier rows always use the
/// array form, per `transcript.rs::is_tool_result_carrier`) or an array of
/// content blocks, mirroring `cmdcrush::extract_tool_results`; each
/// `tool_result` block's own nested `content` field is flattened via
/// [`tool_result_text`], exactly mirroring `cmdcrush::tool_result_text`'s
/// string-vs-array handling.
///
/// **Tool name resolution — a deliberate simplification.** A real Claude
/// API `tool_result` content block carries no tool name at all (only the
/// `tool_use` block that preceded it does, in an earlier row this
/// single-row function never sees — magic-compact resolves this via a
/// `toolNamesById` map built across the whole turn set,
/// `research/features.md` §4). Since `prune_tool_row`'s signature is
/// single-row and context-free, this function instead reads an optional
/// `tool_name` (or `toolName`) field directly on the `tool_result` block —
/// a convention this module defines for its own tests and for a future
/// caller with turn-level context (Phase 4/5's orchestration, which already
/// walks `tool_use`/`tool_result` correspondence per turn) to attach before
/// calling this function. Absent that field, the tool is treated as
/// unclassified and pruned under `DEFAULT_LIMIT_*`.
fn extract_tool_result(row: &TranscriptRow) -> Option<(String, String)> {
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
                        .map(str::to_owned);
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

/// `true` when `text` exceeds the threshold that applies to `tool_name`.
///
/// `Bash` gets a flat char-length cutoff (no word check); the "agent
/// output" class and the default class both use an OR of char length and
/// word count (matching `prune.ts`'s `exceeds()`: "word count OR char
/// count over threshold", `research/features.md` §4).
fn exceeds_threshold(tool_name: &str, text: &str) -> bool {
    let char_len = text.chars().count();

    if tool_name == "Bash" {
        return char_len > BASH_LIMIT_CHARS;
    }

    let word_count = text.split_whitespace().count();
    if AGENT_OUTPUT_TOOL_NAMES.contains(&tool_name) {
        return char_len > AGENT_OUTPUT_LIMIT_CHARS || word_count > AGENT_OUTPUT_LIMIT_WORDS;
    }

    char_len > DEFAULT_LIMIT_CHARS || word_count > DEFAULT_LIMIT_WORDS
}

/// Prune `row`'s tool-result content into `cache` if it exceeds its
/// tool-name's threshold, replacing the content with a placeholder that
/// embeds the resulting `content_id`; otherwise return it unchanged.
///
/// The full, unmodified content is cached (via [`OmissionCache::insert`])
/// **before** the row is rewritten, so a cache-insert failure never leaves
/// content silently dropped — it propagates as `Err` and the row is not
/// touched.
///
/// # Errors
///
/// Returns an error if [`OmissionCache::insert`] fails.
pub fn prune_tool_row(
    row: &TranscriptRow,
    cache: &OmissionCache,
    session_id: &str,
) -> Result<PrunedRow> {
    let Some((tool_name, text)) = extract_tool_result(row) else {
        return Ok(PrunedRow::Unchanged(row.clone()));
    };

    if !exceeds_threshold(&tool_name, &text) {
        return Ok(PrunedRow::Unchanged(row.clone()));
    }

    let content_id = cache.insert(session_id, &tool_name, &text)?;
    let placeholder = format!("[pruned: see read_omitted_content(session_id, \"{content_id}\")]");

    let mut fields = row.fields().clone();
    if let Some(message) = fields.message.as_mut() {
        if let Some(content) = message.get_mut("content") {
            // `message.content` is either a bare string or an array of
            // content blocks (mirroring `extract_tool_result`'s own
            // string-vs-array handling above). For the array form, replace
            // only the matched `tool_result` block(s)' own nested `content`
            // field — overwriting the whole array here would silently drop
            // sibling blocks (other tool_result blocks, text blocks) that
            // happen to share this row.
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

    Ok(PrunedRow::Pruned {
        row: with_fields(row, fields),
        content_id,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn tool_result_row(tool_name: Option<&str>, content: &str) -> TranscriptRow {
        let name_field = tool_name
            .map(|n| format!(r#","tool_name":"{n}""#))
            .unwrap_or_default();
        let line = format!(
            r#"{{"type":"user","uuid":"t1","parentUuid":"a1","isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"x","content":{}{}}}]}}}}"#,
            serde_json::to_string(content).unwrap(),
            name_field,
        );
        serde_json::from_str(&line).unwrap()
    }

    /// `OmissionCache::open` hardens its cache file's *parent directory* to
    /// `0700` on every open (ADR-009). That means the parent must be a
    /// directory the test process actually owns and may chmod — placing the
    /// cache file directly inside the shared, sandbox-restricted OS temp
    /// root (as a bare `NamedTempFile` would) fails with "Operation not
    /// permitted" there. `tempfile::tempdir()` gives each test its own
    /// process-owned directory to nest the cache file under instead.
    fn open_cache() -> (TempDir, OmissionCache) {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("omission-cache.sqlite");
        let cache = OmissionCache::open(&cache_path).unwrap();
        (dir, cache)
    }

    // -- happy path --

    #[test]
    fn prune_tool_row_should_return_pruned_when_content_exceeds_default_char_limit() {
        let (_f, cache) = open_cache();
        let long_content = "x".repeat(DEFAULT_LIMIT_CHARS + 1);
        let row = tool_result_row(Some("SomeTool"), &long_content);

        let result = prune_tool_row(&row, &cache, "session-1").unwrap();

        let PrunedRow::Pruned { row, content_id } = result else {
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

    // -- error path --

    /// A cache pointed at a directory (rather than a writable file) fails
    /// every `insert` call, exercising `prune_tool_row`'s error-propagation
    /// path without needing a fake `OmissionCache` type.
    #[test]
    fn prune_tool_row_should_propagate_err_when_cache_insert_fails() {
        let dir = tempfile::tempdir().unwrap();
        // A path that is itself an existing directory: `Connection::open`
        // succeeds against it in some sqlite builds but every subsequent
        // write fails, which is exactly the "insert fails" shape this test
        // needs. To keep this robust across sqlite builds, use a path
        // rooted under a *read-only* parent directory instead, which fails
        // deterministically inside `OmissionCache::open`'s own
        // `create_dir_all`/permission step, and covers the same
        // "cache construction/write path fails, error must propagate"
        // property `prune_tool_row` needs to honor.
        let readonly_parent = dir.path().join("readonly");
        std::fs::create_dir(&readonly_parent).unwrap();
        std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        let cache_path = readonly_parent.join("nested").join("omission-cache.sqlite");

        let cache_result = OmissionCache::open(&cache_path);
        assert!(
            cache_result.is_err(),
            "expected OmissionCache::open to fail under a read-only parent, proving the \
             error path this test exercises actually triggers"
        );

        // Directly exercise prune_tool_row's propagation of an insert
        // failure using a cache that opened successfully but whose next
        // auto-assigned content_id has been made to collide with the
        // `PRIMARY KEY (session_id, content_id)` constraint. Note: making
        // the on-disk file read-only *after* `OmissionCache::open` does
        // *not* reliably force a write failure — POSIX permission checks
        // happen at `open()`, and the connection's file descriptor (plus
        // its already-created WAL file) was opened while the file was
        // still writable, so subsequent writes through that fd succeed
        // regardless of a later `chmod`. A real constraint violation is
        // the deterministic, portable way to force `insert()` to fail.
        let dir2 = tempfile::TempDir::new().unwrap();
        let cache_path = dir2.path().join("omission-cache.sqlite");
        let cache = OmissionCache::open(&cache_path).unwrap();

        // `insert`'s next content_id for a session with `count` existing
        // rows is `omitted-{count+1:03}`. Pre-seed one row directly via a
        // separate connection (bypassing `OmissionCache::insert`, the only
        // way to get an out-of-sequence content_id into the table) whose
        // own content_id is `omitted-002` — i.e. the id `insert` will
        // compute for a session that already has exactly 1 row. The real
        // `insert` call below then recomputes `omitted-002` (count = 1) and
        // collides with this pre-seeded row's PRIMARY KEY.
        {
            let raw = rusqlite::Connection::open(&cache_path).unwrap();
            raw.execute(
                "INSERT INTO omitted_content (session_id, content_id, content, tool_name, created_at) \
                 VALUES ('session-1', 'omitted-002', 'pre-seeded', 'SomeTool', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let long_content = "y".repeat(DEFAULT_LIMIT_CHARS + 1);
        let row = tool_result_row(Some("SomeTool"), &long_content);

        let result = prune_tool_row(&row, &cache, "session-1");
        assert!(
            result.is_err(),
            "insert failure must propagate as Err, not silently drop content"
        );
    }

    // -- threshold table --

    #[test]
    #[allow(clippy::too_many_lines)] // table-driven test over one cohesive threshold matrix
    fn prune_tool_row_should_match_threshold_table_for_bash_default_and_agent_output_classes() {
        struct Case {
            tool_name: &'static str,
            char_count: usize,
            word_count: usize,
            expect_pruned: bool,
        }

        let cases = [
            // Bash: flat char cutoff only.
            Case {
                tool_name: "Bash",
                char_count: BASH_LIMIT_CHARS,
                word_count: 1,
                expect_pruned: false,
            },
            Case {
                tool_name: "Bash",
                char_count: BASH_LIMIT_CHARS + 1,
                word_count: 1,
                expect_pruned: true,
            },
            // Default class: char OR word limit.
            Case {
                tool_name: "SomeTool",
                char_count: DEFAULT_LIMIT_CHARS,
                word_count: 1,
                expect_pruned: false,
            },
            Case {
                tool_name: "SomeTool",
                char_count: DEFAULT_LIMIT_CHARS + 1,
                word_count: 1,
                expect_pruned: true,
            },
            Case {
                tool_name: "SomeTool",
                char_count: 10,
                word_count: DEFAULT_LIMIT_WORDS,
                expect_pruned: false,
            },
            Case {
                tool_name: "SomeTool",
                char_count: 10,
                word_count: DEFAULT_LIMIT_WORDS + 1,
                expect_pruned: true,
            },
            // Agent output class: higher char OR word limit.
            Case {
                tool_name: "Agent",
                char_count: AGENT_OUTPUT_LIMIT_CHARS,
                word_count: 1,
                expect_pruned: false,
            },
            Case {
                tool_name: "Agent",
                char_count: AGENT_OUTPUT_LIMIT_CHARS + 1,
                word_count: 1,
                expect_pruned: true,
            },
            Case {
                tool_name: "TaskOutput",
                char_count: 10,
                word_count: AGENT_OUTPUT_LIMIT_WORDS,
                expect_pruned: false,
            },
            Case {
                tool_name: "TaskOutput",
                char_count: 10,
                word_count: AGENT_OUTPUT_LIMIT_WORDS + 1,
                expect_pruned: true,
            },
        ];

        for case in cases {
            let (_f, cache) = open_cache();
            // Build content with an exact word count: `word_count` single
            // "w" tokens space-separated, then pad with filler chars (a
            // non-whitespace suffix) to reach exactly `char_count` chars
            // without perturbing the word count.
            let words: Vec<&str> = std::iter::repeat_n("w", case.word_count).collect();
            let mut content = words.join(" ");
            // `case.char_count` is a floor, not a hard cap: for the
            // word-threshold cases (e.g. word_count=128, char_count=10) the
            // words alone already exceed it, and truncating to hit it
            // exactly would cut words and corrupt the very word count the
            // case is testing. Pad up to the floor when the words fall
            // short of it; otherwise leave the natural, word-count-exact
            // content as-is (its char count exceeding the floor is fine —
            // these cases test the word threshold, not the char one, and
            // the natural length still stays well under the char limit for
            // its class).
            if content.chars().count() < case.char_count {
                let pad_len = case.char_count - content.chars().count();
                content.push('_');
                content.push_str(&"a".repeat(pad_len.saturating_sub(1)));
            }
            assert!(
                content.chars().count() >= case.char_count,
                "test setup: char count below requested floor"
            );
            assert_eq!(
                content.split_whitespace().count(),
                case.word_count,
                "test setup: word count mismatch"
            );

            let row = tool_result_row(Some(case.tool_name), &content);
            let result = prune_tool_row(&row, &cache, "session-1").unwrap();

            match (&result, case.expect_pruned) {
                (PrunedRow::Pruned { content_id, .. }, true) => {
                    let cached = cache.get("session-1", content_id).unwrap();
                    assert_eq!(
                        cached,
                        Some(content.clone()),
                        "cached content for tool {} must be byte-identical",
                        case.tool_name
                    );
                }
                (PrunedRow::Unchanged(_), false) => {}
                (PrunedRow::Pruned { .. }, false) => {
                    panic!(
                        "tool {} chars={} words={} expected Unchanged but was Pruned",
                        case.tool_name, case.char_count, case.word_count
                    );
                }
                (PrunedRow::Unchanged(_), true) => {
                    panic!(
                        "tool {} chars={} words={} expected Pruned but was Unchanged",
                        case.tool_name, case.char_count, case.word_count
                    );
                }
            }
        }
    }
}
