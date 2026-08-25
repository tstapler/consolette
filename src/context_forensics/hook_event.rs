//! `consolette context-hook <event-name>` — a fast, fire-and-forget
//! subcommand invoked by consolette's own installed Claude Code hooks
//! (`hooks_install.rs`). Reads the hook's JSON payload from stdin, inserts
//! one [`HookEventRow`], and exits `0` (plan.md Story 4.2.1).
//!
//! Never blocks the tool call it's attached to on an ingestion failure: a
//! malformed or empty payload logs a warning and still exits `0`, same as
//! any other skipped/malformed input elsewhere in this codebase
//! (`transcript.rs`'s own house style).

use std::io::Read;

use crate::context_forensics::store::{ContextForensicsStore, HookEventRow};

/// Reads `stdin` fully, parses it as JSON to extract `session_id` (present
/// in every real Claude Code hook payload; absent only for a malformed
/// input), and inserts one [`HookEventRow`] with `event_kind` set to
/// `event`. Never fails the caller: a missing/unparseable payload logs a
/// `tracing::warn!` and returns `Ok(())` without inserting a row, matching
/// the "never block the tool call" observability requirement.
///
/// # Errors
///
/// Returns an error if `stdin` can't be read or the store insert fails —
/// never for a malformed/empty payload, which is handled internally.
pub fn handle_hook_event(
    store: &ContextForensicsStore,
    event: &str,
    stdin: &mut impl Read,
) -> anyhow::Result<()> {
    let mut payload = String::new();
    stdin.read_to_string(&mut payload)?;
    let payload = payload.trim();

    if payload.is_empty() {
        tracing::warn!(event, "context-hook received empty stdin, skipping");
        return Ok(());
    }

    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(event, %error, "context-hook received malformed JSON, skipping");
            return Ok(());
        }
    };

    let session_id = parsed
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    store.insert_hook_event(&HookEventRow {
        session_id,
        event_kind: event.to_string(),
        payload: payload.to_string(),
        received_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_store(dir: &TempDir) -> ContextForensicsStore {
        ContextForensicsStore::open(&dir.path().join("store.sqlite")).unwrap()
    }

    #[test]
    fn handle_hook_event_should_insert_one_row_when_payload_valid() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        let mut stdin = std::io::Cursor::new(r#"{"session_id": "s1", "cwd": "/tmp"}"#);

        handle_hook_event(&store, "PostToolUse", &mut stdin).unwrap();

        let events = store.hook_events_for_session("s1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_kind, "PostToolUse");
    }

    #[test]
    fn handle_hook_event_should_skip_and_not_error_when_stdin_empty() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        let mut stdin = std::io::Cursor::new("");

        handle_hook_event(&store, "SessionStart", &mut stdin).unwrap();

        assert_eq!(store.hook_events_for_session("s1").unwrap().len(), 0);
    }

    #[test]
    fn handle_hook_event_should_skip_and_not_error_when_payload_malformed() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        let mut stdin = std::io::Cursor::new("not json {{{");

        handle_hook_event(&store, "PostToolUse", &mut stdin).unwrap();

        assert_eq!(store.hook_events_for_session("s1").unwrap().len(), 0);
    }
}
