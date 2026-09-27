//! `syntax_variance` — detects, stores, and reports unimplemented or experimental
//! HTTP headers, request body keys, and response shapes (ADR-001).

pub mod inspector;
pub mod mcp_server;
pub mod store;

pub use inspector::{inspect_headers, inspect_request_body, SyntaxVariance};
pub use mcp_server::{handle_get_syntax_variances, owns_tool, tool_defs, TOOL_NAME_GET_SYNTAX_VARIANCES};
pub use store::{get_variances, record_variances, SyntaxVarianceRecord};
