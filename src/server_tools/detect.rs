//! Detection + rewrite of Anthropic server-tool definitions (Story 1.1.1).
//!
//! Pure functions over `serde_json::Value`: no I/O, no routing knowledge.
//! The entrypoint calls [`rewrite_for_upstream`] before dispatching to a
//! non-executing upstream, and [`has_server_web_search_def`] as the branch
//! gate.

use serde_json::{json, Value};

/// Tool-definition `name` shared by the native server tool and our synthetic
/// upstream-callable replacement.
pub const SERVER_TOOL_NAME: &str = "web_search";

/// Versioned `type` strings observed in the wild. Detection additionally
/// accepts any future `web_search_*` type so a new Anthropic version keeps
/// working (validation: `detect_should_match_all_known_and_future_versions`).
pub const KNOWN_SERVER_TOOL_VERSIONS: &[&str] = &[
    "web_search_20250305",
    "web_search_20260209",
    "web_search_20260318",
];

/// Synthetic function description. Non-empty on purpose: an empty,
/// parameter-less function trips the Cohere 400 class
/// (`the 'web_search' tool must have at least a description, input, or
/// output`), and the rewrite pairs it with a non-empty `input_schema`, so
/// the class is impossible on the emulation path (S-2).
pub const SYNTHETIC_DESCRIPTION: &str = "Search the web for current or external information. Provide a concise search query; returns ranked results with title, URL, and text snippet.";

/// Rename target for a user function coincidentally named `web_search`
/// (adversarial-review minor): keeps it callable without colliding with the
/// synthetic def.
pub const USER_COLLISION_RENAME: &str = "web_search_user";

/// Extracted per-request hints from a server def. Carried in a sidecar —
/// never sent upstream. Domain/location filters are logged-and-ignored in V1
/// (see the lossy-mapping table in `super`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerWebSearchDef {
    pub max_uses: Option<u32>,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
    pub has_location_hint: bool,
}

impl ServerWebSearchDef {
    /// Whether any V1-ignored filter was present (drives the per-request log
    /// line in the entrypoint).
    #[must_use]
    pub fn has_ignored_filters(&self) -> bool {
        !self.allowed_domains.is_empty()
            || !self.blocked_domains.is_empty()
            || self.has_location_hint
    }
}

/// Whether `type` identifies a server web-search tool definition: any known
/// versioned string, the bare `web_search` type, or any future
/// `web_search_*` version.
#[must_use]
pub fn is_server_tool_type(tool_type: &str) -> bool {
    tool_type == "web_search"
        || tool_type.starts_with("web_search_")
        || KNOWN_SERVER_TOOL_VERSIONS.contains(&tool_type)
}

/// Whether `tool` is a server `web_search` definition: a `web_search_*` type
/// with `name == "web_search"`. A user function coincidentally named
/// `web_search` (function type, or no type at all) is NOT a server def.
#[must_use]
pub fn is_server_web_search_def(tool: &Value) -> bool {
    let name_matches = tool.get("name").and_then(Value::as_str) == Some(SERVER_TOOL_NAME);
    let type_matches = tool
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(is_server_tool_type);
    name_matches && type_matches
}

/// Whether an Anthropic request body carries at least one server `web_search`
/// def in `tools[]`.
#[must_use]
pub fn has_server_web_search_def(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(is_server_web_search_def))
}

/// Extract the sidecar hints from one server def (missing fields → defaults).
#[must_use]
pub fn extract_server_def(tool: &Value) -> ServerWebSearchDef {
    let strings = |key: &str| -> Vec<String> {
        tool.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    ServerWebSearchDef {
        max_uses: tool
            .get("max_uses")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        allowed_domains: strings("allowed_domains"),
        blocked_domains: strings("blocked_domains"),
        has_location_hint: tool.get("user_location").is_some(),
    }
}

/// The synthetic upstream-callable function replacing one server def.
#[must_use]
pub fn synthetic_function_def() -> Value {
    json!({
        "name": SERVER_TOOL_NAME,
        "description": SYNTHETIC_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to execute"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }
    })
}

/// Rewrite an Anthropic request body for a non-executing upstream:
///
/// - every server `web_search` def is swapped for the synthetic function def
///   (sidecar hints returned separately, never sent upstream);
/// - a user function coincidentally named `web_search` is renamed to
///   `web_search_user` so it cannot collide with the synthetic def;
/// - every other tool passes through untouched;
/// - message history is back-converted server-shape → function-shape
///   (previous emulated turns echoed back by the client), via
///   [`crate::server_tools::mapping::convert_history_for_upstream`].
///
/// Returns the rewritten body plus one sidecar per replaced server def.
#[must_use]
pub fn rewrite_for_upstream(body: &Value) -> (Value, Vec<ServerWebSearchDef>) {
    let mut rewritten = body.clone();
    let mut sidecars = Vec::new();

    if let Some(tools) = rewritten.get_mut("tools").and_then(Value::as_array_mut) {
        let mut out = Vec::with_capacity(tools.len());
        for tool in tools.drain(..) {
            if is_server_web_search_def(&tool) {
                sidecars.push(extract_server_def(&tool));
                out.push(synthetic_function_def());
            } else if tool.get("name").and_then(Value::as_str) == Some(SERVER_TOOL_NAME) {
                let mut renamed = tool;
                if let Some(obj) = renamed.as_object_mut() {
                    obj.insert(
                        "name".to_string(),
                        Value::String(USER_COLLISION_RENAME.to_string()),
                    );
                }
                out.push(renamed);
            } else {
                out.push(tool);
            }
        }
        *tools = out;
    }

    if let Some(messages) = rewritten.get("messages").cloned() {
        rewritten["messages"] =
            crate::server_tools::mapping::convert_history_for_upstream(&messages);
    }

    (rewritten, sidecars)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server_def(tool_type: &str) -> Value {
        json!({"type": tool_type, "name": "web_search"})
    }

    #[test]
    fn detect_should_match_all_known_and_future_versions() {
        for known in KNOWN_SERVER_TOOL_VERSIONS {
            assert!(
                is_server_web_search_def(&server_def(known)),
                "known version {known} must detect"
            );
        }
        // Future Anthropic version, unseen at implementation time.
        assert!(is_server_web_search_def(&server_def("web_search_20270401")));
        // Bare type is also treated as a server def (lenient).
        assert!(is_server_web_search_def(&server_def("web_search")));
    }

    #[test]
    fn detect_should_reject_user_function_named_web_search() {
        let function_def = json!({
            "name": "web_search",
            "input_schema": {"type": "object"}
        });
        assert!(!is_server_web_search_def(&function_def));

        let other_server_tool =
            json!({"type": "code_execution_20250522", "name": "code_execution"});
        assert!(!is_server_web_search_def(&other_server_tool));

        let wrong_name = json!({"type": "web_search_20250305", "name": "web_fetch"});
        assert!(!is_server_web_search_def(&wrong_name));
    }

    #[test]
    fn rewrite_should_emit_described_nonempty_schema_when_server_def_given() {
        let body = json!({
            "model": "cohere/north-mini-code:free",
            "messages": [{"role": "user", "content": "search?"}],
            "tools": [server_def("web_search_20250305")]
        });

        let (rewritten, sidecars) = rewrite_for_upstream(&body);

        assert_eq!(sidecars.len(), 1);
        let def = &rewritten["tools"][0];
        assert_eq!(def["name"], json!("web_search"));
        assert!(
            def["description"].as_str().is_some_and(|d| !d.is_empty()),
            "synthetic def must carry a description (Cohere 400 class)"
        );
        assert!(
            def["input_schema"]
                .as_object()
                .is_some_and(|s| !s.is_empty()),
            "synthetic def must carry a non-empty input_schema (Cohere 400 class)"
        );
        // Original server def is gone from the outbound body.
        assert!(
            !rewritten["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(is_server_web_search_def),
            "no server def may leak upstream"
        );
    }

    #[test]
    fn rewrite_should_pass_through_other_tools_and_rename_collision() {
        let other = json!({"name": "get_time", "input_schema": {"type": "object"}});
        let collision = json!({"name": "web_search", "input_schema": {"type": "object"}});
        let body = json!({
            "model": "m",
            "messages": [],
            "tools": [server_def("web_search_20260209"), other.clone(), collision]
        });

        let (rewritten, sidecars) = rewrite_for_upstream(&body);

        assert_eq!(sidecars.len(), 1);
        let tools = rewritten["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert!(tools.contains(&other));
        assert!(
            tools
                .iter()
                .any(|t| t["name"] == json!(USER_COLLISION_RENAME)),
            "colliding user function must be renamed, not dropped: {tools:?}"
        );
    }

    #[test]
    fn extract_should_capture_max_uses_and_filters() {
        let tool = json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 3,
            "allowed_domains": ["sec.gov"],
            "user_location": {"type": "approximate", "city": "Seattle"}
        });

        let def = extract_server_def(&tool);

        assert_eq!(def.max_uses, Some(3));
        assert_eq!(def.allowed_domains, vec!["sec.gov".to_string()]);
        assert!(def.has_ignored_filters());
    }
}
