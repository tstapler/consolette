//! MCP server exposing context-forensics queries over stdio (plan.md
//! Story 5.1.1) — lets a Claude Code session ask "where did my tokens go"
//! without opening the dashboard.
//!
//! Hand-written [`ServerHandler`] + `dispatch`-by-tool-name, matching
//! `claude_code_session::mcp_server::CompactionMcpServer`'s style (Pattern
//! Decisions: tool count stays small enough that manual dispatch reads
//! better than the `#[tool]`-macro machinery). Each tool delegates to the
//! same [`ContextForensicsStore`] query methods `server.rs`'s HTTP routes
//! use — no separately-computed data, so a tool's result always agrees
//! with the equivalent `GET /v1/context/...` route.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::{service::RequestContext, service::RoleServer, ErrorData, ServerHandler};
use serde_json::{json, Value};

use crate::context_forensics::store::ContextForensicsStore;
use crate::cost_metrics::pricing::PricingTable;

pub struct ContextForensicsMcpServer {
    store: Arc<ContextForensicsStore>,
    pricing: Arc<PricingTable>,
}

impl ContextForensicsMcpServer {
    #[must_use]
    pub fn new(store: Arc<ContextForensicsStore>, pricing: Arc<PricingTable>) -> Self {
        Self { store, pricing }
    }

    /// The actual `call_tool` dispatch logic, factored out of
    /// [`ServerHandler::call_tool`] so unit tests can exercise it without a
    /// live `RequestContext<RoleServer>` (mirrors `CompactionMcpServer`'s
    /// same split, for the same reason). `pub` so `main.rs`'s combined
    /// `consolette mcp` server composition (a separate crate) can delegate
    /// to it directly.
    #[must_use]
    pub fn dispatch(&self, request: &CallToolRequestParams) -> CallToolResult {
        if request.name == "get_session_composition" {
            self.get_session_composition(request)
        } else if request.name == "get_session_growth" {
            self.get_session_growth(request)
        } else if request.name == "list_sessions_summary" {
            self.list_sessions_summary()
        } else if request.name == "get_turn_content" {
            self.get_turn_content(request)
        } else {
            CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool: {}",
                request.name
            ))])
        }
    }

    fn get_session_composition(&self, request: &CallToolRequestParams) -> CallToolResult {
        let Some(session_id) = string_arg(request, "session_id") else {
            return missing_args_result("get_session_composition", &["session_id"]);
        };
        if let Some(result) = self.require_known_session(session_id) {
            return result;
        }
        let turns = match self.store.composition_for_session(session_id) {
            Ok(turns) => turns,
            Err(error) => return store_error_result(&error),
        };
        let cross_check_status = self
            .store
            .cross_check_status_for_session(session_id)
            .unwrap_or(crate::context_forensics::store::CrossCheckStatus::TranscriptOnly);
        let cross_check_detail = self
            .store
            .cross_check_detail_for_session(session_id, cross_check_status)
            .unwrap_or(None);
        json_result(&json!({
            "turns": turns,
            "cross_check_status": cross_check_status,
            "cross_check_detail": cross_check_detail,
        }))
    }

    fn get_session_growth(&self, request: &CallToolRequestParams) -> CallToolResult {
        let Some(session_id) = string_arg(request, "session_id") else {
            return missing_args_result("get_session_growth", &["session_id"]);
        };
        if let Some(result) = self.require_known_session(session_id) {
            return result;
        }
        let turns = match self.store.growth_for_session(session_id) {
            Ok(turns) => turns,
            Err(error) => return store_error_result(&error),
        };
        let native_compaction_events = self
            .store
            .native_compaction_events_for_session(session_id)
            .unwrap_or_default();
        json_result(&json!({
            "turns": turns,
            "native_compaction_events": native_compaction_events,
        }))
    }

    fn list_sessions_summary(&self) -> CallToolResult {
        match self.store.summary_for_all_sessions(&self.pricing) {
            Ok(summaries) => json_result(&json!(summaries)),
            Err(error) => store_error_result(&error),
        }
    }

    fn get_turn_content(&self, request: &CallToolRequestParams) -> CallToolResult {
        let Some(session_id) = string_arg(request, "session_id") else {
            return missing_args_result("get_turn_content", &["session_id", "turn_index"]);
        };
        let Some(turn_index) = u64_arg(request, "turn_index") else {
            return missing_args_result("get_turn_content", &["session_id", "turn_index"]);
        };
        if let Some(result) = self.require_known_session(session_id) {
            return result;
        }
        match self.store.turn_content(session_id, turn_index) {
            Ok(Some((turn, calls))) => {
                let tool_rows: Vec<Value> = turn
                    .tool_rows_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                let assistant_rows: Vec<Value> = calls
                    .iter()
                    .filter_map(|call| parsed_json(call.message_json.as_ref()))
                    .collect();
                json_result(&json!({
                    "user_row": parsed_json(turn.user_row_json.as_ref()),
                    "assistant_rows": assistant_rows,
                    "tool_rows": tool_rows,
                }))
            }
            Ok(None) => CallToolResult::error(vec![ContentBlock::text(format!(
                "turn_not_found: session {session_id} has no turn at index {turn_index}"
            ))]),
            Err(error) => store_error_result(&error),
        }
    }

    /// Returns `Some(error result)` if `session_id` isn't a known session
    /// or the lookup itself failed; `None` if the caller should proceed.
    fn require_known_session(&self, session_id: &str) -> Option<CallToolResult> {
        match self.store.get_session(session_id) {
            Ok(Some(_)) => None,
            Ok(None) => Some(CallToolResult::error(vec![ContentBlock::text(format!(
                "session_not_found: {session_id}"
            ))])),
            Err(error) => Some(store_error_result(&error)),
        }
    }

    /// Test-only entry point into [`Self::dispatch`].
    #[cfg(test)]
    fn call_tool_for_test(&self, request: &CallToolRequestParams) -> CallToolResult {
        self.dispatch(request)
    }
}

fn string_arg<'a>(request: &'a CallToolRequestParams, key: &str) -> Option<&'a str> {
    request.arguments.as_ref()?.get(key).and_then(Value::as_str)
}

fn u64_arg(request: &CallToolRequestParams, key: &str) -> Option<u64> {
    request.arguments.as_ref()?.get(key).and_then(Value::as_u64)
}

/// Mirrors `server.rs`'s private `parsed_json` — deserializes a stored
/// `*_json` column back into a [`Value`], so this tool's `user_row`/
/// `assistant_rows` shape matches `GET /v1/context/sessions/{id}/turns/{n}`
/// exactly.
fn parsed_json(raw: Option<&String>) -> Option<Value> {
    raw.and_then(|s| serde_json::from_str(s).ok())
}

fn json_result(value: &Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn store_error_result(error: &anyhow::Error) -> CallToolResult {
    tracing::warn!(%error, "context-forensics MCP tool: store query failed");
    CallToolResult::error(vec![ContentBlock::text("store_query_failed".to_string())])
}

fn missing_args_result(tool_name: &str, required: &[&str]) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{tool_name} requires arguments: {}",
        required.join(", ")
    ))])
}

fn session_id_schema_property() -> Value {
    json!({
        "type": "string",
        "description": "The context-forensics session id (as returned by list_sessions_summary)."
    })
}

fn get_session_composition_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": { "session_id": session_id_schema_property() },
        "required": ["session_id"]
    });
    Tool::new(
        "get_session_composition",
        "Per-turn tool-I/O / conversation / system token composition for one session.",
        schema.as_object().cloned().unwrap_or_default(),
    )
}

fn get_session_growth_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": { "session_id": session_id_schema_property() },
        "required": ["session_id"]
    });
    Tool::new(
        "get_session_growth",
        "Per-turn cumulative context-token growth for one session, with native-compaction markers.",
        schema.as_object().cloned().unwrap_or_default(),
    )
}

fn list_sessions_summary_tool() -> Tool {
    let schema = json!({ "type": "object", "properties": {} });
    Tool::new(
        "list_sessions_summary",
        "Cross-session cost/call, peak-context, and coverage summary for every stored session.",
        schema.as_object().cloned().unwrap_or_default(),
    )
}

/// This server's tool definitions, exposed standalone so
/// `main.rs::CombinedMcpServer` can list them without needing a live
/// `RequestContext<RoleServer>`.
#[must_use]
pub fn tool_defs() -> Vec<Tool> {
    vec![
        get_session_composition_tool(),
        get_session_growth_tool(),
        list_sessions_summary_tool(),
        get_turn_content_tool(),
    ]
}

/// `true` if `name` is a tool this server owns — lets a composite server
/// route a `call_tool` request without duplicating this server's own tool
/// list.
#[must_use]
pub fn owns_tool(name: &str) -> bool {
    matches!(
        name,
        "get_session_composition"
            | "get_session_growth"
            | "list_sessions_summary"
            | "get_turn_content"
    )
}

fn get_turn_content_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "session_id": session_id_schema_property(),
            "turn_index": {
                "type": "integer",
                "minimum": 0,
                "description": "The turn's index within the session (0-based)."
            }
        },
        "required": ["session_id", "turn_index"]
    });
    Tool::new(
        "get_turn_content",
        "The verbatim user/assistant/tool-row content captured for one turn.",
        schema.as_object().cloned().unwrap_or_default(),
    )
}

impl ServerHandler for ContextForensicsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("consolette-context-forensics", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tool_defs(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.dispatch(&request))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::context_forensics::store::{SessionRow, Source, TurnRow};
    use tempfile::TempDir;

    fn server_with_store(dir: &TempDir) -> (ContextForensicsMcpServer, Arc<ContextForensicsStore>) {
        let store =
            Arc::new(ContextForensicsStore::open(&dir.path().join("store.sqlite")).unwrap());
        let pricing = Arc::new(PricingTable::load_default());
        (
            ContextForensicsMcpServer::new(store.clone(), pricing),
            store,
        )
    }

    fn call(tool_name: &str, args: &[(&str, Value)]) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        let mut map = serde_json::Map::new();
        for (key, value) in args {
            map.insert((*key).to_string(), value.clone());
        }
        params.arguments = Some(map);
        params
    }

    fn seed_session(store: &ContextForensicsStore, id: &str) {
        store
            .upsert_session(&SessionRow {
                id: id.to_string(),
                source: Source::ClaudeCode,
                path: format!("/tmp/{id}.jsonl"),
                project: None,
                started_at: Some("2026-01-01T00:00:00Z".to_string()),
                last_ingested_at: "2026-01-01T00:05:00Z".to_string(),
                chain_coverage_ratio: Some(1.0),
                parse_failure_count: 0,
            })
            .unwrap();
    }

    #[test]
    fn dispatch_should_return_error_when_tool_name_unknown() {
        let dir = TempDir::new().unwrap();
        let (server, _store) = server_with_store(&dir);

        let result = server.call_tool_for_test(&call("not_a_real_tool", &[]));

        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn get_session_composition_should_match_http_route_shape_when_session_has_turns() {
        let dir = TempDir::new().unwrap();
        let (server, store) = server_with_store(&dir);
        seed_session(&store, "s1");
        store
            .upsert_turn(&TurnRow {
                id: "t1".to_string(),
                session_id: "s1".to_string(),
                turn_index: 0,
                user_row_uuid: "u1".to_string(),
                cumulative_tokens: 100,
                user_row_json: None,
                tool_rows_json: None,
            })
            .unwrap();

        let result = server.call_tool_for_test(&call(
            "get_session_composition",
            &[("session_id", json!("s1"))],
        ));

        assert_ne!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content block");
        };
        let body: Value = serde_json::from_str(&text.text).unwrap();
        let expected = store.composition_for_session("s1").unwrap();
        assert_eq!(
            body,
            json!({ "turns": expected, "cross_check_status": "transcript_only", "cross_check_detail": null })
        );
    }

    #[test]
    fn get_session_composition_should_return_error_when_session_unknown() {
        let dir = TempDir::new().unwrap();
        let (server, _store) = server_with_store(&dir);

        let result = server.call_tool_for_test(&call(
            "get_session_composition",
            &[("session_id", json!("does-not-exist"))],
        ));

        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn get_session_growth_should_include_turns_and_native_compaction_events() {
        let dir = TempDir::new().unwrap();
        let (server, store) = server_with_store(&dir);
        seed_session(&store, "s1");
        store
            .upsert_turn(&TurnRow {
                id: "t1".to_string(),
                session_id: "s1".to_string(),
                turn_index: 0,
                user_row_uuid: "u1".to_string(),
                cumulative_tokens: 100,
                user_row_json: None,
                tool_rows_json: None,
            })
            .unwrap();

        let result =
            server.call_tool_for_test(&call("get_session_growth", &[("session_id", json!("s1"))]));

        assert_ne!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content block");
        };
        let body: Value = serde_json::from_str(&text.text).unwrap();
        assert!(body.get("turns").is_some());
        assert!(body.get("native_compaction_events").is_some());
    }

    #[test]
    fn list_sessions_summary_should_return_empty_array_when_no_sessions() {
        let dir = TempDir::new().unwrap();
        let (server, _store) = server_with_store(&dir);

        let result = server.call_tool_for_test(&call("list_sessions_summary", &[]));

        assert_ne!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content block");
        };
        let body: Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(body, json!([]));
    }

    #[test]
    fn get_turn_content_should_return_error_when_missing_required_args() {
        let dir = TempDir::new().unwrap();
        let (server, _store) = server_with_store(&dir);

        let result =
            server.call_tool_for_test(&call("get_turn_content", &[("session_id", json!("s1"))]));

        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn get_turn_content_should_return_error_when_turn_index_out_of_range() {
        let dir = TempDir::new().unwrap();
        let (server, store) = server_with_store(&dir);
        seed_session(&store, "s1");

        let result = server.call_tool_for_test(&call(
            "get_turn_content",
            &[("session_id", json!("s1")), ("turn_index", json!(99))],
        ));

        assert_eq!(result.is_error, Some(true));
    }
}
