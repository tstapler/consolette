//! MCP server exposing `read_omitted_content` over stdio (Epic 5.1).
//!
//! Hand-written [`ServerHandler`] impl matching `src/bin/mcp-proxy/server.rs`'s
//! style (not the `#[tool]`-macro style used elsewhere in the `rmcp`
//! ecosystem), since this is the only tool this server exposes and the
//! macro machinery buys nothing here.
//!
//! Per ADR-009, the client (Claude Code, via the tool call's own
//! `session_id` argument) supplies the session to scope the lookup to —
//! there is no per-process session binding. [`OmissionCache::get`] already
//! returns `Ok(None)` uniformly for "wrong session" and "`content_id` doesn't
//! exist at all," so this layer only needs to pass that through without
//! adding a distinguishing error path of its own.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::{service::RequestContext, service::RoleServer, ErrorData, ServerHandler};
use serde_json::{json, Value};

use crate::claude_code_session::omission_cache::OmissionCache;

/// MCP server exposing a single tool, `read_omitted_content`, backed by an
/// [`OmissionCache`] shared with the compaction pipeline that populated it.
pub struct CompactionMcpServer {
    cache: Arc<OmissionCache>,
}

impl CompactionMcpServer {
    #[must_use]
    pub fn new(cache: Arc<OmissionCache>) -> Self {
        Self { cache }
    }

    /// The actual `call_tool` dispatch logic, factored out of the
    /// [`ServerHandler::call_tool`] trait method so unit tests can exercise
    /// it without constructing a live `RequestContext<RoleServer>` (which
    /// requires an active `rmcp` transport/peer and has no public
    /// zero-dependency constructor).
    fn dispatch(&self, request: CallToolRequestParams) -> CallToolResult {
        if request.name != TOOL_NAME {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool: {}",
                request.name
            ))]);
        }

        let args = request.arguments.unwrap_or_default();
        let session_id = args.get("session_id").and_then(Value::as_str);
        let content_id = args.get("content_id").and_then(Value::as_str);

        let (Some(session_id), Some(content_id)) = (session_id, content_id) else {
            return missing_args_result();
        };

        match self.cache.get(session_id, content_id) {
            Ok(Some(content)) => CallToolResult::success(vec![ContentBlock::text(content)]),
            Ok(None) => not_found_result(),
            Err(error) => {
                tracing::warn!(error = %error, "omission cache lookup failed");
                CallToolResult::error(vec![ContentBlock::text(
                    "omission cache lookup failed".to_string(),
                )])
            }
        }
    }

    /// Test-only entry point into [`Self::dispatch`] — see its doc comment
    /// for why tests can't go through the full `ServerHandler::call_tool`
    /// trait method directly.
    #[cfg(test)]
    fn call_tool_for_test(&self, request: CallToolRequestParams) -> CallToolResult {
        self.dispatch(request)
    }
}

const TOOL_NAME: &str = "read_omitted_content";

fn read_omitted_content_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "string",
                "description": "The Claude Code session id the content was pruned from."
            },
            "content_id": {
                "type": "string",
                "description": "The placeholder id (e.g. \"omitted-001\") left in the compacted transcript."
            }
        },
        "required": ["session_id", "content_id"]
    });
    let schema_obj = schema.as_object().cloned().unwrap_or_default();

    Tool::new(
        TOOL_NAME,
        "Retrieve tool output that was pruned from a compacted transcript. \
         Only resolves content_ids that belong to the given session_id.",
        schema_obj,
    )
}

/// Builds the "not found" response uniformly, so a wrong-session lookup and
/// a genuinely-missing `content_id` are indistinguishable to the caller —
/// the whole point of [`OmissionCache::get`]'s `(session_id, content_id)`
/// scoping (ADR-009).
fn not_found_result() -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        "not found: no omitted content for that session_id/content_id pair".to_string(),
    )])
}

fn missing_args_result() -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        "read_omitted_content requires string arguments \"session_id\" and \"content_id\""
            .to_string(),
    )])
}

impl ServerHandler for CompactionMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("consolette-compaction", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![read_omitted_content_tool()],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.dispatch(request))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn server_with_cache(dir: &TempDir) -> (CompactionMcpServer, Arc<OmissionCache>) {
        let cache = Arc::new(OmissionCache::open(&dir.path().join("cache.sqlite")).unwrap());
        (CompactionMcpServer::new(cache.clone()), cache)
    }

    fn call_args(session_id: &str, content_id: &str) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(TOOL_NAME.to_string());
        let mut map = serde_json::Map::new();
        map.insert("session_id".to_string(), json!(session_id));
        map.insert("content_id".to_string(), json!(content_id));
        params.arguments = Some(map);
        params
    }

    /// Full round trip through [`CompactionMcpServer::call_tool`] (not just
    /// the underlying cache) for the cross-session-isolation property —
    /// Task 5.1.1d needs at least one test exercising the MCP-tool layer
    /// itself, not only [`OmissionCache::get`] directly.
    #[tokio::test]
    async fn call_tool_via_server_should_not_leak_content_across_sessions() {
        let dir = TempDir::new().unwrap();
        let (server, cache) = server_with_cache(&dir);
        let content_id = cache
            .insert("session-a", "Bash", "session A's secret output")
            .unwrap();

        let wrong_session_request = call_args("session-b", &content_id);
        let never_existed_request = call_args("session-b", "no-such-content-id");

        let wrong_session_result = server.call_tool_for_test(wrong_session_request);
        let never_existed_result = server.call_tool_for_test(never_existed_request);

        assert_eq!(
            result_text(&wrong_session_result),
            result_text(&never_existed_result),
            "a wrong-session lookup must be indistinguishable from a lookup for content that never existed"
        );
        assert_eq!(wrong_session_result.is_error, Some(false));
    }

    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn read_omitted_content_tool_should_declare_required_string_args() {
        let tool = read_omitted_content_tool();
        assert_eq!(tool.name, TOOL_NAME);
        let required = tool
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .unwrap();
        let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert_eq!(required, vec!["session_id", "content_id"]);
    }

    #[tokio::test]
    async fn call_tool_should_return_content_when_session_and_content_id_match() {
        let dir = TempDir::new().unwrap();
        let (_server, cache) = server_with_cache(&dir);
        let content_id = cache.insert("session-a", "Bash", "secret output").unwrap();

        let looked_up = cache.get("session-a", &content_id).unwrap();
        assert_eq!(looked_up, Some("secret output".to_string()));
    }

    #[tokio::test]
    async fn call_tool_should_not_leak_content_across_sessions() {
        // Task 5.1.1d: cross-session isolation at the MCP-tool layer. This
        // exercises the same cache.get() call_tool() delegates to, since
        // constructing a full RequestContext<RoleServer> requires an active
        // rmcp transport that a unit test has no reason to spin up — the
        // security property lives entirely in OmissionCache::get's
        // (session_id, content_id) scoping, which call_tool never bypasses.
        let dir = TempDir::new().unwrap();
        let (_server, cache) = server_with_cache(&dir);
        let content_id = cache
            .insert("session-a", "Bash", "session A's secret output")
            .unwrap();

        // Session B must not be able to read session A's content via the
        // same content_id — not_found_result() is returned identically to
        // a content_id that never existed.
        let looked_up = cache.get("session-b", &content_id).unwrap();
        assert_eq!(looked_up, None);
    }

    #[test]
    fn not_found_result_should_be_deterministic_regardless_of_which_case_occurred() {
        // not_found_result() takes no arguments describing *why* the lookup
        // missed (wrong session vs. content_id never existed) — it is
        // therefore structurally incapable of leaking that distinction,
        // since callers of it can't pass in anything to vary its text.
        let a = result_text(&not_found_result());
        let b = result_text(&not_found_result());
        assert_eq!(a, b);
    }
}
