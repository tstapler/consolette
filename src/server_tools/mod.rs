//! In-proxy emulation of Anthropic `web_search` server tools.
//!
//! Claude Code sends server tool definitions (e.g.
//! `{"type": "web_search_20250305", "name": "web_search"}` — no
//! `input_schema`) on `POST /v1/messages`. Upstreams that cannot execute
//! server tools (OpenAI-compatible, Cohere-backed models, Gemini, Bedrock)
//! would otherwise 400 or ignore them, so the proxy rewrites the server def
//! into a synthetic upstream-callable function, executes searches itself via
//! stapler-mcp over the rmcp stdio seam, and maps the final turn back to the
//! faithful `server_tool_use` + `web_search_tool_result` shape.
//!
//! Owner decisions (recorded in
//! `project_plans/server-tool-emulation/implementation/plan.md`, win over any
//! conflicting acceptance text):
//!
//! - **D1**: emulate everywhere, Bedrock included. Only Anthropic-kind routes
//!   keep native passthrough.
//! - **D2**: `max_iterations = 5` default, hard ceiling 10; per-search 15s,
//!   total 120s; pool size 2.
//! - **D3**: `max_results` config maps to the Brave `count` argument.
//! - **D4**: fallback chain `brave_web_search` → `browser_web_search` →
//!   drop-degrade. Scraping lives in stapler-mcp (new `browser_web_search`
//!   tool, [stapler-mcp#46](https://github.com/tstapler/stapler-mcp/issues/46));
//!   consolette only orchestrates the chain. Fallback triggers on the
//!   missing-key error and 401/403 only; 429/5xx propagate as executor errors.
//!
//! ## Lossy-mapping table (V1 fidelity limits, by design)
//!
//! | Native field | Emulated mapping |
//! |---|---|
//! | `server_tool_use` block | Synthesized with deterministic id `srvtoolu_emul_<iter>_<idx>` |
//! | `web_search_tool_result.content[].encrypted_content` | **Not minted** — we cannot forge Anthropic's encrypted blobs; results carry `title` + `url` + `text` |
//! | `citations` attachments | Dropped (no source blobs to cite) |
//! | `allowed_domains` / `blocked_domains` / `user_location` | Logged-and-ignored in V1 (per-request log line); Brave-side filtering is a follow-up |
//! | `max_uses` | Honored as an iteration cap (`min(max_uses, max_iterations, 10)`) |
//! | `allowed_callers: ["direct"]` (ZDR marker) | Silently ignored in V1 |
//! | Per-result `description` over the snippet cap | Truncated with a `…(truncated)` marker |
//!
//! ## Backend contract (pinned by tests in `executor.rs`)
//!
//! - Binary: `stapler-mcp` on `PATH` or at the configured absolute path.
//! - Tools: `brave_web_search` and `browser_web_search`, input
//!   `{query: string, count?: number}`, output
//!   `{results: [{title, url, description}]}`.
//! - Missing-key detection matches stapler-mcp's stable
//!   `"BRAVE_API_KEY is not set"` error string.
//!
//! `BRAVE_API_KEY` never enters this process: it stays in the daemon's
//! environment (requirements.md C-1). Nothing here logs or returns secrets.

pub mod detect;
pub mod executor;
pub mod r#loop;
pub mod mapping;
pub mod sse;

pub use detect::{
    has_server_web_search_def, is_server_web_search_def, rewrite_for_upstream, ServerWebSearchDef,
};
pub use executor::{
    classify_backend_error, parse_search_results, BackendClass, ErrorKind, ExecutorError,
    McpSearchPool, PoolConfig, ProdConnector, SearchBackend, SearchExecutor, SearchOutcome,
    SearchResult, BRAVE_TOOL_NAME, BROWSER_TOOL_NAME, MISSING_KEY_MESSAGE,
};
pub use mapping::{
    aggregate_usage, build_final_turn, convert_history_for_upstream, extract_usage_pair,
    find_other_tool_calls, find_web_search_calls, server_block_id, to_error_tool_result,
    to_function_tool_result, to_server_tool_use, to_web_search_tool_result, truncate_with_marker,
    UpstreamSearchCall,
};
pub use r#loop::{run_loop, Dispatch, FnDispatch, LoopLimits, LoopOutcome};
pub use sse::synthesize_sse;

/// Hard ceiling on emulated search iterations per client request
/// (pre-mortem failure 1: runaway spend on a looping model). No config knob
/// may exceed this.
pub const HARD_CEILING_ITERATIONS: u32 = 10;

/// Tuning knobs for server-tool emulation (`[server_tools]` table). All
/// optional; a missing table yields safe defaults. Carries no secrets by
/// construction (C-1) — see `config_should_carry_no_secrets_when_snapshot`.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerToolsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_backend_path")]
    pub backend_path: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_per_search_timeout_ms")]
    pub per_search_timeout_ms: u64,
    #[serde(default = "default_total_timeout_ms")]
    pub total_timeout_ms: u64,
    #[serde(default = "default_browser_timeout_ms")]
    pub browser_timeout_ms: u64,
    #[serde(default = "default_max_results")]
    pub max_results: u32,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
}

fn default_enabled() -> bool {
    true
}
fn default_backend_path() -> String {
    "stapler-mcp".to_string()
}
fn default_max_iterations() -> u32 {
    5
}
fn default_per_search_timeout_ms() -> u64 {
    15_000
}
fn default_total_timeout_ms() -> u64 {
    120_000
}
fn default_browser_timeout_ms() -> u64 {
    30_000
}
fn default_max_results() -> u32 {
    5
}
fn default_pool_size() -> usize {
    2
}

impl Default for ServerToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            backend_path: default_backend_path(),
            max_iterations: default_max_iterations(),
            per_search_timeout_ms: default_per_search_timeout_ms(),
            total_timeout_ms: default_total_timeout_ms(),
            browser_timeout_ms: default_browser_timeout_ms(),
            max_results: default_max_results(),
            pool_size: default_pool_size(),
        }
    }
}

impl ServerToolsConfig {
    /// Loop limits derived from this config, with the iteration cap clamped
    /// to [`HARD_CEILING_ITERATIONS`] (D2).
    #[must_use]
    pub fn loop_limits(&self) -> LoopLimits {
        LoopLimits {
            max_iterations: self.max_iterations.min(HARD_CEILING_ITERATIONS),
            max_results: self.max_results,
            total_timeout: std::time::Duration::from_millis(self.total_timeout_ms),
        }
    }

    /// Pool wiring derived from this config (D2/D4).
    #[must_use]
    pub fn pool_config(&self) -> PoolConfig {
        PoolConfig {
            binary_path: self.backend_path.clone(),
            pool_size: self.pool_size.max(1),
            per_search_timeout_ms: self.per_search_timeout_ms,
            browser_timeout_ms: self.browser_timeout_ms,
        }
    }
}

/// Per-process emulation state snapshotted once in
/// `EntrypointState::build`: the tuning config plus whether the active
/// route is eligible for emulation (D1: every route with a non-Anthropic
/// upstream; Anthropic-native routes keep passthrough).
#[derive(Debug, Clone)]
pub struct ServerToolsRuntime {
    pub config: ServerToolsConfig,
    pub route_eligible: bool,
}

impl Default for ServerToolsRuntime {
    fn default() -> Self {
        Self {
            config: ServerToolsConfig::default(),
            route_eligible: true,
        }
    }
}

impl ServerToolsRuntime {
    /// Whether `body` should enter the emulation loop: enabled, on an
    /// eligible route, and carrying a server `web_search` def. The
    /// no-server-tools case returns `false` so today's path stays
    /// byte-identical (S-3).
    #[must_use]
    pub fn should_emulate(&self, body: &serde_json::Value) -> bool {
        self.config.enabled && self.route_eligible && has_server_web_search_def(body)
    }

    /// Pure route-eligibility check over a loaded config: `true` unless every
    /// upstream referenced by the active (first) route is Anthropic-kind.
    /// Unknown route/upstream references fail open to emulation (uniform
    /// loop per D1); an empty route table fails closed to passthrough.
    #[must_use]
    pub fn route_eligible_from_config(config: &crate::config::schema::Config) -> bool {
        use crate::config::schema::UpstreamKind;

        let Some(route) = config.routes.first() else {
            return false;
        };
        if route.upstreams.is_empty() {
            return false;
        }
        for referenced in &route.upstreams {
            let kind = config
                .upstreams
                .iter()
                .find(|u| u.name == referenced.name)
                .map(|u| &u.kind);
            match kind {
                Some(UpstreamKind::Anthropic) => {}
                None | Some(_) => return true,
            }
        }
        false
    }
}

/// Backend reachability probe used once at proxy boot for the pre-mortem
/// failure-2 log line ("emulation enabled, backend reachable: yes/no").
///
/// Filesystem-only on purpose: no child is spawned here (lazy pool start on
/// first search). A bare binary name is resolved against `PATH`; an absolute
/// or `./`-relative path must exist as a file.
#[must_use]
pub fn backend_reachable(binary_path: &str) -> bool {
    if binary_path.contains('/') {
        return std::path::Path::new(binary_path).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(binary_path).is_file())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn loop_limits_should_clamp_max_iterations_to_hard_ceiling() {
        let config = ServerToolsConfig {
            max_iterations: 99,
            ..ServerToolsConfig::default()
        };

        assert_eq!(config.loop_limits().max_iterations, HARD_CEILING_ITERATIONS);
    }

    #[test]
    fn config_should_carry_no_secrets_when_snapshot() {
        let config = ServerToolsConfig::default();
        let json = serde_json::to_value(&config).unwrap();
        let debug = format!("{config:?}");

        for haystack in [json.to_string(), debug] {
            assert!(
                !haystack.contains("BRAVE_API_KEY"),
                "server-tools config must never carry search credentials: {haystack}"
            );
        }
        assert_eq!(config, ServerToolsConfig::default());
    }

    #[test]
    fn route_eligible_from_config_should_fail_closed_when_no_routes() {
        let mut config = crate::config::schema::Config::default();
        config.routes.clear();

        assert!(!ServerToolsRuntime::route_eligible_from_config(&config));
    }

    #[test]
    fn route_eligible_from_config_should_emulate_for_mixed_route() {
        // `Config::default()` ships anthropic + bedrock on the default route.
        let config = crate::config::schema::Config::default();

        assert!(ServerToolsRuntime::route_eligible_from_config(&config));
    }

    #[test]
    fn backend_reachable_should_find_sh_on_path_and_reject_missing_binary() {
        assert!(backend_reachable("sh"));
        assert!(!backend_reachable("consolette-definitely-not-a-binary"));
    }
}
