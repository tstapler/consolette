//! MCP tool server for syntax variances.

use serde_json::{json, Value};
use crate::context_forensics::store::ContextForensicsStore;

pub const TOOL_NAME_GET_SYNTAX_VARIANCES: &str = "get_syntax_variances";

#[must_use]
pub fn tool_defs() -> Vec<rmcp::model::Tool> {
    let schema = json!({
        "type": "object",
        "properties": {}
    });
    let schema_obj = schema.as_object().cloned().unwrap_or_default();

    vec![rmcp::model::Tool::new(
        TOOL_NAME_GET_SYNTAX_VARIANCES,
        "List all detected client/provider HTTP header and API payload syntax variances or unmapped beta flags.",
        schema_obj,
    )]
}

#[must_use]
pub fn owns_tool(name: &str) -> bool {
    name == TOOL_NAME_GET_SYNTAX_VARIANCES
}

pub fn handle_get_syntax_variances(store: &ContextForensicsStore) -> Result<Value, anyhow::Error> {
    let records = super::store::get_variances(store)?;
    Ok(json!({
        "variances": records,
        "count": records.len()
    }))
}
