//! stapler-mcp search executor over seam A (Story 2.1.1 + 2.1.3).
//!
//! Consolette never touches search APIs or keys directly (requirements.md
//! C-1): it spawns the `stapler-mcp` binary as a stdio MCP child (rmcp
//! client, already a dependency) and calls tools over JSON-RPC. A small
//! bounded pool of persistent children amortizes spawn cost; per-call
//! timeouts plus a consecutive-failure circuit breaker keep backend outages
//! from ever failing client requests.
//!
//! ## Fallback chain (D4)
//!
//! Per search: `brave_web_search` → (on missing-key/auth error only)
//! `browser_web_search` → (on browser failure) executor error, which the
//! loop renders as error-content and degrades. 429/5xx/timeouts propagate as
//! executor errors with NO browser call (no silent masking of billing/rate
//! signals). Scraping lives in stapler-mcp; consolette only orchestrates.
//!
//! The live-browser path is developed against the [`fake`] in-process MCP
//! server plus contract tests here. The live end-to-end fallback test is
//! explicitly **deferred** until stapler-mcp ships `browser_web_search`
//! ([stapler-mcp#46](https://github.com/tstapler/stapler-mcp/issues/46)) —
//! see the `#[ignore]`d test in `tests/server_tool_emulation.rs`.
//!
//! Health model: rmcp 2.2 exposes no client→server `ping`, so the spawn-time
//! `list_tools` probe (which also pins the `brave_web_search` tool's
//! presence) serves as the liveness check; failed slots are evicted and
//! re-spawned lazily.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{serve_client, serve_server, Peer, RoleClient, RoleServer};
use serde_json::Value;
use tokio::sync::Mutex;

/// MCP tool name for Brave-backed search (pinned contract).
pub const BRAVE_TOOL_NAME: &str = "brave_web_search";
/// MCP tool name for stapler-mcp-side browser search (pinned contract,
/// shipped in stapler-mcp — see `super` docs; live use deferred on
/// [stapler-mcp#46](https://github.com/tstapler/stapler-mcp/issues/46)).
pub const BROWSER_TOOL_NAME: &str = "browser_web_search";
/// Stable missing-key error string emitted by stapler-mcp when
/// `BRAVE_API_KEY` is unset. Pinned by contract test; drives D4 fallback.
pub const MISSING_KEY_MESSAGE: &str = "BRAVE_API_KEY is not set";

/// Consecutive search failures before the circuit opens (fail-fast degrade
/// until the cooldown elapses).
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// How long an open circuit stays open before a trial search is allowed.
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(60);

/// One search hit in the uniform shape both backends share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
}

/// Which backend served a search (metrics label `brave` vs `browser`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBackend {
    Brave,
    Browser,
}

impl SearchBackend {
    /// Stable metrics/log label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SearchBackend::Brave => "brave",
            SearchBackend::Browser => "browser",
        }
    }
}

/// A completed search plus the backend that served it.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub backend: SearchBackend,
}

/// Machine-checkable failure kinds for executor errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    MissingBinary,
    SpawnFailed,
    Timeout,
    ToolMissing,
    BadResponse,
    Backend,
    CircuitOpen,
}

/// An executor failure. By construction this type can never be a
/// `ProviderError` and never touches `HealthRegistry` — the loop renders it
/// as error-content and degrades (validation:
/// `executor_failure_should_not_trip_cooldown_when_backend_down`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ExecutorError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ExecutorError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Whether this backend error is eligible for the D4 browser fallback:
    /// missing-key/auth only, never rate/billing signals.
    #[must_use]
    pub fn fallback_eligible(&self) -> bool {
        self.kind == ErrorKind::Backend
            && matches!(
                classify_backend_error(&self.message),
                BackendClass::MissingKeyOrAuth
            )
    }
}

/// Classification of a backend tool-error string (pure; D4 fallback rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendClass {
    MissingKeyOrAuth,
    RateLimited,
    ServerError,
    Other,
}

/// Whether `message` contains `code` as a standalone token (split on
/// non-alphanumeric boundaries), so a port number like 14015 never
/// false-positives as a 401 status.
fn has_status_token(message: &str, code: &str) -> bool {
    message
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == code)
}

fn has_server_error_token(message: &str) -> bool {
    message
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| {
            token.len() == 3 && token.starts_with('5') && token.chars().all(|c| c.is_ascii_digit())
        })
}

/// Classify a backend error string per the D4 rule: the stable missing-key
/// message or 401/403 fall back; 429/5xx propagate.
///
/// Check order matters: the missing-key string wins first (a daemon message
/// could embed a port number), then 429, then 5xx, then auth markers.
#[must_use]
pub fn classify_backend_error(message: &str) -> BackendClass {
    if message.contains(MISSING_KEY_MESSAGE) {
        return BackendClass::MissingKeyOrAuth;
    }
    if has_status_token(message, "429") {
        return BackendClass::RateLimited;
    }
    if has_server_error_token(message) {
        return BackendClass::ServerError;
    }
    let lowered = message.to_lowercase();
    if has_status_token(message, "401")
        || has_status_token(message, "403")
        || lowered.contains("unauthorized")
        || lowered.contains("forbidden")
        || lowered.contains("invalid api key")
    {
        return BackendClass::MissingKeyOrAuth;
    }
    BackendClass::Other
}

/// Parse a tool payload into the uniform result shape. Accepts the pinned
/// contract `{results: [{title, url, description}]}` or, tolerantly, a bare
/// array of hits. Missing hit fields default to `""`; non-object items are
/// skipped. Anything else is [`ErrorKind::BadResponse`].
///
/// # Errors
///
/// Returns `BadResponse` when no results array can be found.
pub fn parse_search_results(payload: &Value) -> Result<Vec<SearchResult>, ExecutorError> {
    let items = match payload {
        Value::Object(_) => payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ExecutorError::new(
                    ErrorKind::BadResponse,
                    format!("missing `results` array in tool payload: {payload}"),
                )
            })?,
        Value::Array(items) => items,
        _ => {
            return Err(ExecutorError::new(
                ErrorKind::BadResponse,
                format!("tool payload is neither object nor array: {payload}"),
            ));
        }
    };
    Ok(items
        .iter()
        .filter_map(Value::as_object)
        .map(|hit| SearchResult {
            title: hit
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            url: hit
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            description: hit
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect())
}

/// Concatenate a tool result's text blocks (falling back to its structured
/// payload when there is no text).
fn result_text(result: &CallToolResult) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in &result.content {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.as_str());
        }
    }
    if parts.is_empty() {
        if let Some(structured) = &result.structured_content {
            return structured.to_string();
        }
    }
    parts.join("\n")
}

/// Extract the tool payload: structured content when present, else the text
/// blocks parsed as JSON.
fn result_payload(result: &CallToolResult) -> Result<Value, ExecutorError> {
    if let Some(structured) = &result.structured_content {
        return Ok(structured.clone());
    }
    let text = result_text(result);
    serde_json::from_str(&text).map_err(|e| {
        ExecutorError::new(
            ErrorKind::BadResponse,
            format!("tool result is not JSON: {e}: {text}"),
        )
    })
}

/// Bounded search execution over seam A. Implementations are shared across
/// requests (`Send + Sync`).
#[async_trait]
pub trait SearchExecutor: Send + Sync {
    /// Execute one search (`count` = D3 `max_results` mapping). Never fails
    /// the client request at the type level — errors become loop-level
    /// error-content or drop-degrade.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError`] on any backend failure (missing binary,
    /// timeout, missing tool, bad payload, backend error, open circuit).
    async fn search(&self, query: &str, count: u32) -> Result<SearchOutcome, ExecutorError>;

    /// Fast liveness check used for the loop's drop-degrade fast path
    /// (binary missing / tool missing ⇒ skip emulation entirely).
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError`] when the backend cannot serve searches.
    async fn probe(&self) -> Result<(), ExecutorError>;
}

/// How a pool slot gets its MCP connection. Production spawns the binary;
/// tests connect to the [`fake`] in-process server.
#[async_trait]
pub trait BackendConnector: Send + Sync {
    /// Establish one MCP connection (spawn + handshake + tool-presence
    /// probe).
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError`] when the backend cannot be reached.
    async fn connect(&self) -> Result<PooledConn, ExecutorError>;
}

/// One live MCP connection: the client service (owns the transport task),
/// its peer handle, and the child process when one was spawned.
pub struct PooledConn {
    _service: RunningService<RoleClient, ()>,
    peer: Peer<RoleClient>,
    child: Option<tokio::process::Child>,
}

/// Production connector: spawn `binary_path` as a stdio MCP child.
pub struct ProdConnector {
    binary_path: String,
    handshake_timeout: Duration,
}

impl ProdConnector {
    #[must_use]
    pub fn new(binary_path: String, handshake_timeout: Duration) -> Self {
        Self {
            binary_path,
            handshake_timeout,
        }
    }
}

#[async_trait]
impl BackendConnector for ProdConnector {
    async fn connect(&self) -> Result<PooledConn, ExecutorError> {
        use std::process::Stdio;

        let mut child = tokio::process::Command::new(&self.binary_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ExecutorError::new(
                        ErrorKind::MissingBinary,
                        format!("search backend binary not found: {}", self.binary_path),
                    )
                } else {
                    ExecutorError::new(
                        ErrorKind::SpawnFailed,
                        format!("failed to spawn {}: {e}", self.binary_path),
                    )
                }
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ExecutorError::new(
                ErrorKind::SpawnFailed,
                "child stdin unavailable".to_string(),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ExecutorError::new(
                ErrorKind::SpawnFailed,
                "child stdout unavailable".to_string(),
            )
        })?;
        let transport =
            rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(stdout, stdin);
        let service = serve_client((), transport).await.map_err(|e| {
            ExecutorError::new(ErrorKind::SpawnFailed, format!("MCP handshake failed: {e}"))
        })?;
        let peer = service.peer().clone();
        verify_brave_tool(&peer, self.handshake_timeout).await?;
        Ok(PooledConn {
            _service: service,
            peer,
            child: Some(child),
        })
    }
}

/// Confirm the backend actually offers `brave_web_search` (contract probe —
/// a stapler-mcp upgrade that renames the tool fails fast here, pre-mortem
/// failure 2).
///
/// # Errors
///
/// Returns `Timeout` or `ToolMissing` when the probe fails.
async fn verify_brave_tool(
    peer: &Peer<RoleClient>,
    timeout: Duration,
) -> Result<(), ExecutorError> {
    let tools = tokio::time::timeout(timeout, peer.list_tools(None))
        .await
        .map_err(|_| {
            ExecutorError::new(ErrorKind::Timeout, "list_tools probe timed out".to_string())
        })?
        .map_err(|e| ExecutorError::new(ErrorKind::Backend, format!("list_tools failed: {e}")))?;
    if tools.tools.iter().any(|t| t.name == BRAVE_TOOL_NAME) {
        Ok(())
    } else {
        Err(ExecutorError::new(
            ErrorKind::ToolMissing,
            format!("backend does not offer `{BRAVE_TOOL_NAME}` (contract drift?)"),
        ))
    }
}

/// Pool wiring (D2 defaults live in [`crate::server_tools::ServerToolsConfig`]).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub binary_path: String,
    pub pool_size: usize,
    pub per_search_timeout_ms: u64,
    pub browser_timeout_ms: u64,
}

/// Bounded pool of persistent stdio MCP children (seam A). Lazy start (zero
/// cost for non-search traffic), evict-on-error, consecutive-failure circuit
/// breaker. The only shared resource in the emulation path.
pub struct McpSearchPool<C = ProdConnector> {
    config: PoolConfig,
    connector: C,
    slots: Vec<Mutex<Option<PooledConn>>>,
    next_slot: AtomicUsize,
    consecutive_failures: AtomicUsize,
    circuit_opened_at: StdMutex<Option<Instant>>,
}

impl McpSearchPool<ProdConnector> {
    /// Production pool from wiring config (spawns nothing — lazy start).
    #[must_use]
    pub fn new(config: PoolConfig) -> Self {
        let connector = ProdConnector::new(
            config.binary_path.clone(),
            Duration::from_millis(config.per_search_timeout_ms),
        );
        Self::with_connector(config, connector)
    }
}

impl<C: BackendConnector> McpSearchPool<C> {
    /// Pool over an explicit connector (tests use [`fake::FakeConnector`]).
    pub fn with_connector(config: PoolConfig, connector: C) -> Self {
        let slots = (0..config.pool_size.max(1))
            .map(|_| Mutex::new(None))
            .collect();
        Self {
            config,
            connector,
            slots,
            next_slot: AtomicUsize::new(0),
            consecutive_failures: AtomicUsize::new(0),
            circuit_opened_at: StdMutex::new(None),
        }
    }

    fn circuit_open(&self) -> bool {
        if self.consecutive_failures.load(Ordering::Relaxed) < MAX_CONSECUTIVE_FAILURES as usize {
            return false;
        }
        let opened = self
            .circuit_opened_at
            .lock()
            .is_ok_and(|guard| (*guard).is_some_and(|at| at.elapsed() < CIRCUIT_COOLDOWN));
        opened
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.circuit_opened_at.lock() {
            *guard = None;
        }
    }

    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= MAX_CONSECUTIVE_FAILURES as usize {
            if let Ok(mut guard) = self.circuit_opened_at.lock() {
                if guard.is_none() {
                    *guard = Some(Instant::now());
                }
            }
        }
    }

    /// Check out a live connection from the round-robin slot, spawning and
    /// probing on first use. The slot guard is held for the whole search so
    /// concurrent searches fan out over children, bounded by pool size.
    ///
    /// # Errors
    ///
    /// Returns the connector/probe [`ExecutorError`] when the backend is down.
    async fn checkout(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<PooledConn>>, ExecutorError> {
        if self.circuit_open() {
            return Err(ExecutorError::new(
                ErrorKind::CircuitOpen,
                "search circuit open after consecutive failures".to_string(),
            ));
        }
        let slot = self.next_slot.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut guard = self.slots[slot].lock().await;
        if guard.is_none() {
            match self.connector.connect().await {
                Ok(conn) => {
                    self.record_success();
                    *guard = Some(conn);
                }
                Err(e) => {
                    self.record_failure();
                    return Err(e);
                }
            }
        }
        Ok(guard)
    }

    /// Drop a broken slot (kills the child when one was spawned).
    async fn evict(guard: &mut Option<PooledConn>) {
        if let Some(mut conn) = guard.take() {
            if let Some(mut child) = conn.child.take() {
                let _ = child.kill().await;
            }
        }
    }

    fn call_args(query: &str, count: u32) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("query".to_string(), Value::String(query.to_string())),
            ("count".to_string(), Value::from(count)),
        ])
    }

    /// Call one MCP tool with a timeout, mapping transport/timeout/tool
    /// failures to [`ExecutorError`].
    async fn call_tool(
        peer: &Peer<RoleClient>,
        tool: &str,
        query: &str,
        count: u32,
        timeout_ms: u64,
    ) -> Result<CallToolResult, ExecutorError> {
        let params = CallToolRequestParams::new(tool.to_string())
            .with_arguments(Self::call_args(query, count));
        let peer = peer.clone();
        tokio::time::timeout(Duration::from_millis(timeout_ms), peer.call_tool(params))
            .await
            .map_err(|_| {
                ExecutorError::new(
                    ErrorKind::Timeout,
                    format!("`{tool}` call timed out after {timeout_ms}ms"),
                )
            })?
            .map_err(|e| {
                ExecutorError::new(ErrorKind::Backend, format!("`{tool}` call failed: {e}"))
            })
    }

    /// Execute the D4 chain for one search: Brave, then (missing-key/auth
    /// only) the browser tool. Returns the outcome with the serving backend.
    async fn execute_chain(
        &self,
        peer: &Peer<RoleClient>,
        query: &str,
        count: u32,
    ) -> Result<SearchOutcome, ExecutorError> {
        let brave = Self::call_tool(
            peer,
            BRAVE_TOOL_NAME,
            query,
            count,
            self.config.per_search_timeout_ms,
        )
        .await;
        let brave = match brave {
            Ok(result) => result,
            Err(e) if e.kind == ErrorKind::Timeout => return Err(e),
            Err(e) => {
                return Err(ExecutorError::new(
                    ErrorKind::Backend,
                    format!("`{BRAVE_TOOL_NAME}` transport failure: {}", e.message),
                ));
            }
        };
        if brave.is_error != Some(true) {
            let results = parse_search_results(&result_payload(&brave)?)?;
            return Ok(SearchOutcome {
                results,
                backend: SearchBackend::Brave,
            });
        }
        let message = result_text(&brave);
        if !matches!(
            classify_backend_error(&message),
            BackendClass::MissingKeyOrAuth
        ) {
            // 429/5xx/other: propagate with NO browser call (D4).
            return Err(ExecutorError::new(ErrorKind::Backend, message));
        }
        let browser = Self::call_tool(
            peer,
            BROWSER_TOOL_NAME,
            query,
            count,
            self.config.browser_timeout_ms,
        )
        .await?;
        if browser.is_error == Some(true) {
            return Err(ExecutorError::new(
                ErrorKind::Backend,
                format!(
                    "browser fallback failed after Brave auth failure ({message}): {}",
                    result_text(&browser)
                ),
            ));
        }
        let results = parse_search_results(&result_payload(&browser)?)?;
        Ok(SearchOutcome {
            results,
            backend: SearchBackend::Browser,
        })
    }
}

#[async_trait]
impl<C: BackendConnector> SearchExecutor for McpSearchPool<C> {
    async fn search(&self, query: &str, count: u32) -> Result<SearchOutcome, ExecutorError> {
        let mut guard = self.checkout().await?;
        let peer = guard
            .as_ref()
            .map(|conn| conn.peer.clone())
            .ok_or_else(|| {
                ExecutorError::new(
                    ErrorKind::SpawnFailed,
                    "slot connection missing".to_string(),
                )
            })?;
        match self.execute_chain(&peer, query, count).await {
            Ok(outcome) => {
                self.record_success();
                Ok(outcome)
            }
            Err(e) => {
                // Transport-level and timeout failures poison the slot; auth
                // and payload failures do not (the connection is healthy).
                if matches!(
                    e.kind,
                    ErrorKind::Timeout | ErrorKind::SpawnFailed | ErrorKind::ToolMissing
                ) || e.message.contains("transport failure")
                {
                    Self::evict(&mut guard).await;
                }
                self.record_failure();
                Err(e)
            }
        }
    }

    async fn probe(&self) -> Result<(), ExecutorError> {
        let guard = self.checkout().await?;
        if guard.is_some() {
            Ok(())
        } else {
            Err(ExecutorError::new(
                ErrorKind::SpawnFailed,
                "slot connection missing".to_string(),
            ))
        }
    }
}

impl<C: BackendConnector> McpSearchPool<C> {
    /// Live probe for the deferred browser tool: checks the connected
    /// backend's tool list for `browser_web_search`. Used by the deferred
    /// live end-to-end test (stapler-mcp#46); doubles as a future startup
    /// probe without changing the lazy pool shape.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError`] when the backend cannot be reached.
    pub async fn live_probe_browser_tool(&self) -> Result<bool, ExecutorError> {
        let guard = self.checkout().await?;
        let peer = guard
            .as_ref()
            .map(|conn| conn.peer.clone())
            .ok_or_else(|| {
                ExecutorError::new(
                    ErrorKind::SpawnFailed,
                    "slot connection missing".to_string(),
                )
            })?;
        let tools = tokio::time::timeout(
            Duration::from_millis(self.config.per_search_timeout_ms),
            peer.list_tools(None),
        )
        .await
        .map_err(|_| {
            ExecutorError::new(
                ErrorKind::Timeout,
                "browser-tool probe timed out".to_string(),
            )
        })?
        .map_err(|e| ExecutorError::new(ErrorKind::Backend, format!("list_tools failed: {e}")))?;
        Ok(tools.tools.iter().any(|t| t.name == BROWSER_TOOL_NAME))
    }
}

/// In-process fake stapler-mcp server + connector for hermetic tests.
///
/// The fake speaks real MCP over a duplex transport (same `serve_client`
/// path as production), pins both tool contracts, and records call counts
/// so tests can assert the D4 chain (e.g. 429 ⇒ zero browser calls).
pub mod fake {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::{
        serve_client, serve_server, verify_brave_tool, BackendConnector, CallToolRequestParams,
        CallToolResult, ContentBlock, ErrorKind, ExecutorError, McpSearchPool, PoolConfig,
        PooledConn, RoleClient, RoleServer, SearchResult, BRAVE_TOOL_NAME, BROWSER_TOOL_NAME,
        MISSING_KEY_MESSAGE,
    };
    use rmcp::model::{ServerCapabilities, ServerInfo};
    use rmcp::service::RequestContext;
    use rmcp::ServerHandler;

    /// Scripted backend behavior per test.
    #[derive(Debug, Clone)]
    pub enum FakeBehavior {
        /// Brave succeeds with these results.
        BraveOk(Vec<SearchResult>),
        /// Brave fails with the stable missing-key string; browser then
        /// serves the paired results (D4 happy path).
        BraveMissingKeyThenBrowser(Vec<SearchResult>),
        /// Brave fails with a 429; the browser tool exists but must NOT be
        /// called (D4 propagation rule).
        BraveRateLimited,
        /// Brave succeeds with a non-JSON text payload.
        BraveBadPayload,
        /// `list_tools` omits `brave_web_search` (contract drift).
        MissingBraveTool,
        /// Brave never responds (timeout path).
        BraveHang,
        /// Browser tool fails after a Brave auth failure (drop-degrade).
        BrowserDown,
    }

    fn results_payload(results: &[SearchResult]) -> Value {
        json!({
            "results": results.iter().map(|r| {
                json!({"title": r.title, "url": r.url, "description": r.description})
            }).collect::<Vec<_>>()
        })
    }

    /// Shared fake-server state: scripted behavior plus call counters.
    /// `Clone` is manual because atomics are not `Clone`; counters restart
    /// from the current values (connections share state via the `Arc`
    /// alias, not via this impl).
    pub struct FakeSearchServer {
        behavior: FakeBehavior,
        pub brave_calls: AtomicUsize,
        pub browser_calls: AtomicUsize,
        concurrent: AtomicUsize,
        pub max_concurrent: AtomicUsize,
    }

    impl FakeSearchServer {
        #[must_use]
        pub fn new(behavior: FakeBehavior) -> Self {
            Self {
                behavior,
                brave_calls: AtomicUsize::new(0),
                browser_calls: AtomicUsize::new(0),
                concurrent: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
            }
        }

        fn track_concurrent(&self) -> ConcurrentGuard<'_> {
            let current = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(current, Ordering::SeqCst);
            ConcurrentGuard { server: self }
        }

        fn brave_result(&self, args: &Value) -> CallToolResult {
            // Pinned input-shape assertion: every Brave call must carry a
            // string `query` (and, when present, a numeric `count`).
            let shape_ok = args.get("query").and_then(Value::as_str).is_some()
                && args.get("count").is_none_or(Value::is_number);
            if !shape_ok {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "invalid arguments shape: {args}"
                ))]);
            }
            match &self.behavior {
                FakeBehavior::BraveOk(results) => {
                    CallToolResult::success(vec![ContentBlock::text(
                        results_payload(results).to_string(),
                    )])
                }
                FakeBehavior::BraveMissingKeyThenBrowser(_) => {
                    CallToolResult::error(vec![ContentBlock::text(MISSING_KEY_MESSAGE)])
                }
                FakeBehavior::BraveRateLimited => CallToolResult::error(vec![ContentBlock::text(
                    "Brave API error 429: rate limit exceeded",
                )]),
                FakeBehavior::BraveBadPayload => {
                    CallToolResult::success(vec![ContentBlock::text("not json{{{")])
                }
                FakeBehavior::MissingBraveTool => {
                    CallToolResult::error(vec![ContentBlock::text("no such tool")])
                }
                FakeBehavior::BraveHang => CallToolResult::success(vec![ContentBlock::text("{}")]),
                FakeBehavior::BrowserDown => CallToolResult::error(vec![ContentBlock::text(
                    "Brave API error 401: unauthorized",
                )]),
            }
        }

        fn browser_result(&self) -> CallToolResult {
            match &self.behavior {
                FakeBehavior::BraveMissingKeyThenBrowser(results) => {
                    CallToolResult::success(vec![ContentBlock::text(
                        results_payload(results).to_string(),
                    )])
                }
                FakeBehavior::BrowserDown => CallToolResult::error(vec![ContentBlock::text(
                    "no system Chrome: browser unavailable",
                )]),
                _ => CallToolResult::success(vec![ContentBlock::text(
                    results_payload(&[]).to_string(),
                )]),
            }
        }

        fn tool_def(name: &str) -> Value {
            json!({
                "name": name,
                "description": format!("fake {name} (Brave-mirroring contract)"),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "count": {"type": "number"}
                    },
                    "required": ["query"]
                }
            })
        }
    }

    struct ConcurrentGuard<'a> {
        server: &'a FakeSearchServer,
    }

    impl Drop for ConcurrentGuard<'_> {
        fn drop(&mut self) {
            self.server.concurrent.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Clone for FakeSearchServer {
        fn clone(&self) -> Self {
            Self {
                behavior: self.behavior.clone(),
                brave_calls: AtomicUsize::new(self.brave_calls.load(Ordering::SeqCst)),
                browser_calls: AtomicUsize::new(self.browser_calls.load(Ordering::SeqCst)),
                concurrent: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(self.max_concurrent.load(Ordering::SeqCst)),
            }
        }
    }

    /// Handle alias shared across connections so counters accumulate
    /// pool-wide in tests.
    pub type SharedFakeServer = Arc<FakeSearchServer>;

    struct FakeHandler {
        server: SharedFakeServer,
    }

    impl ServerHandler for FakeHandler {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl std::future::Future<
            Output = Result<rmcp::model::ListToolsResult, rmcp::model::ErrorData>,
        > + Send
               + '_ {
            let mut tools = vec![FakeSearchServer::tool_def(BROWSER_TOOL_NAME)];
            if !matches!(self.server.behavior, FakeBehavior::MissingBraveTool) {
                tools.insert(0, FakeSearchServer::tool_def(BRAVE_TOOL_NAME));
            }
            // Hardcoded fixtures: a parse failure means this module's own
            // fixture is malformed, so skip the bad entry rather than fail
            // the handshake (the `verify_brave_tool` probe then reports the
            // missing tool as contract drift).
            let tools: Vec<rmcp::model::Tool> = tools
                .into_iter()
                .filter_map(|t| serde_json::from_value(t).ok())
                .collect();
            let result: Result<rmcp::model::ListToolsResult, rmcp::model::ErrorData> =
                serde_json::from_value(json!({ "tools": tools })).map_err(|_| {
                    rmcp::model::ErrorData::internal_error("fake tools fixture malformed", None)
                });
            async move { result }
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::model::ErrorData>> + Send + '_
        {
            let server = Arc::clone(&self.server);
            async move {
                let _tracked = server.track_concurrent();
                if request.name == BRAVE_TOOL_NAME {
                    server.brave_calls.fetch_add(1, Ordering::SeqCst);
                    if matches!(server.behavior, FakeBehavior::BraveHang) {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    let args = request.arguments.map_or(Value::Null, Value::Object);
                    Ok(server.brave_result(&args))
                } else if request.name == BROWSER_TOOL_NAME {
                    server.browser_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(server.browser_result())
                } else {
                    Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "unknown tool {}",
                        request.name
                    ))]))
                }
            }
        }
    }

    /// Connector that dials the in-process fake instead of spawning a child.
    pub struct FakeConnector {
        pub server: SharedFakeServer,
    }

    #[async_trait]
    impl BackendConnector for FakeConnector {
        async fn connect(&self) -> Result<PooledConn, ExecutorError> {
            // Both ends handshake concurrently: `serve_server` waits for the
            // client's initialize and vice versa, so awaiting either one
            // first would deadlock.
            let (client_side, server_side) = tokio::io::duplex(256 * 1024);
            let (server_read, server_write) = tokio::io::split(server_side);
            let server_transport =
                rmcp::transport::async_rw::AsyncRwTransport::<RoleServer, _, _>::new(
                    server_read,
                    server_write,
                );
            let (client_read, client_write) = tokio::io::split(client_side);
            let client_transport =
                rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
                    client_read,
                    client_write,
                );
            let handler = FakeHandler {
                server: Arc::clone(&self.server),
            };
            let (server_result, client_result) = tokio::join!(
                serve_server(handler, server_transport),
                serve_client((), client_transport)
            );
            let server_service = server_result.map_err(|e| {
                ExecutorError::new(
                    ErrorKind::SpawnFailed,
                    format!("fake server start failed: {e}"),
                )
            })?;
            tokio::spawn(async move {
                let _ = server_service.waiting().await;
            });
            let service = client_result.map_err(|e| {
                ExecutorError::new(
                    ErrorKind::SpawnFailed,
                    format!("fake handshake failed: {e}"),
                )
            })?;
            let peer = service.peer().clone();
            verify_brave_tool(&peer, Duration::from_secs(5)).await?;
            Ok(PooledConn {
                _service: service,
                peer,
                child: None,
            })
        }
    }

    /// Build a pool wired to the fake server (hermetic; no binary, no net).
    /// Returns the pool plus the shared server handle for call-count
    /// assertions.
    #[must_use]
    pub fn fake_pool(
        behavior: FakeBehavior,
        per_search_timeout_ms: u64,
        browser_timeout_ms: u64,
        pool_size: usize,
    ) -> (McpSearchPool<FakeConnector>, SharedFakeServer) {
        let server = Arc::new(FakeSearchServer::new(behavior));
        let pool = McpSearchPool::with_connector(
            PoolConfig {
                binary_path: "fake".to_string(),
                pool_size,
                per_search_timeout_ms,
                browser_timeout_ms,
            },
            FakeConnector {
                server: Arc::clone(&server),
            },
        );
        (pool, server)
    }

    /// Sample hits used across contract tests.
    #[must_use]
    pub fn sample_results() -> Vec<SearchResult> {
        vec![
            SearchResult {
                title: "Rust async book".to_string(),
                url: "https://example.com/rust-async".to_string(),
                description: "A guide to async Rust.".to_string(),
            },
            SearchResult {
                title: "Tokio docs".to_string(),
                url: "https://example.com/tokio".to_string(),
                description: "Tokio reference.".to_string(),
            },
        ]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::fake::{fake_pool, sample_results, FakeBehavior};
    use super::*;

    #[test]
    fn classify_should_fall_back_only_on_missing_key_and_auth() {
        assert_eq!(
            classify_backend_error("holder: BRAVE_API_KEY is not set"),
            BackendClass::MissingKeyOrAuth
        );
        assert_eq!(
            classify_backend_error("Brave API error 401: unauthorized"),
            BackendClass::MissingKeyOrAuth
        );
        assert_eq!(
            classify_backend_error("HTTP 403 forbidden"),
            BackendClass::MissingKeyOrAuth
        );
        assert_eq!(
            classify_backend_error("Brave API error 429: rate limit exceeded"),
            BackendClass::RateLimited
        );
        assert_eq!(
            classify_backend_error("upstream 503 unavailable"),
            BackendClass::ServerError
        );
        assert_eq!(
            classify_backend_error("connection reset by peer"),
            BackendClass::Other
        );
        // A large port/id number must not false-positive as a status code.
        assert_eq!(
            classify_backend_error("listening on port 14015"),
            BackendClass::Other
        );
    }

    #[test]
    fn parse_should_pin_brave_mirroring_contract() {
        let payload = json!({
            "results": [
                {"title": "t", "url": "https://example.com", "description": "d"},
                {"title": "t2", "url": "https://example.com/2"}
            ]
        });

        let results = parse_search_results(&payload).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "t");
        assert_eq!(results[1].description, "");
    }

    #[test]
    fn parse_should_reject_payload_without_results() {
        let err = parse_search_results(&json!({"hits": []})).unwrap_err();

        assert_eq!(err.kind, ErrorKind::BadResponse);
    }

    #[tokio::test]
    async fn pool_should_search_brave_when_key_configured() {
        let (pool, server) = fake_pool(FakeBehavior::BraveOk(sample_results()), 5_000, 5_000, 2);

        let outcome = pool.search("rust async", 5).await.unwrap();

        assert_eq!(outcome.backend, SearchBackend::Brave);
        assert_eq!(outcome.results, sample_results());
        assert_eq!(server.brave_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.browser_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pool_should_fall_back_to_browser_when_brave_key_missing() {
        let (pool, server) = fake_pool(
            FakeBehavior::BraveMissingKeyThenBrowser(sample_results()),
            5_000,
            5_000,
            2,
        );

        let outcome = pool.search("rust async", 5).await.unwrap();

        // Uniform shape regardless of backend (D4 contract).
        assert_eq!(outcome.backend, SearchBackend::Browser);
        assert_eq!(outcome.results, sample_results());
        assert_eq!(server.brave_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.browser_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pool_should_not_call_browser_when_brave_rate_limited() {
        let (pool, server) = fake_pool(FakeBehavior::BraveRateLimited, 5_000, 5_000, 2);

        let err = pool.search("rust async", 5).await.unwrap_err();

        assert_eq!(err.kind, ErrorKind::Backend);
        assert_eq!(server.brave_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            server.browser_calls.load(Ordering::SeqCst),
            0,
            "429 must propagate without a browser call (D4)"
        );
        assert!(!err.fallback_eligible());
    }

    #[tokio::test]
    async fn search_should_timeout_and_continue_when_backend_hangs() {
        let (pool, _) = fake_pool(FakeBehavior::BraveHang, 100, 100, 1);

        let err = pool.search("rust async", 5).await.unwrap_err();

        assert_eq!(err.kind, ErrorKind::Timeout);
    }

    #[tokio::test]
    async fn pool_should_degrade_when_binary_missing() {
        let pool = McpSearchPool::new(PoolConfig {
            binary_path: "/nonexistent/consolette-test-no-binary".to_string(),
            pool_size: 1,
            per_search_timeout_ms: 1_000,
            browser_timeout_ms: 1_000,
        });

        let err = pool.search("q", 5).await.unwrap_err();

        assert_eq!(err.kind, ErrorKind::MissingBinary);
        assert!(pool.probe().await.is_err());
    }

    #[tokio::test]
    async fn pool_should_surface_contract_drift_when_brave_tool_missing() {
        let (pool, _) = fake_pool(FakeBehavior::MissingBraveTool, 5_000, 5_000, 1);

        let err = pool.search("q", 5).await.unwrap_err();

        assert_eq!(err.kind, ErrorKind::ToolMissing);
    }

    #[tokio::test]
    async fn breaker_should_skip_emulation_when_failures_consecutive() {
        let (pool, _) = fake_pool(FakeBehavior::BraveBadPayload, 5_000, 5_000, 1);

        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            let _ = pool.search("q", 5).await;
        }
        let err = pool.search("q", 5).await.unwrap_err();

        assert_eq!(err.kind, ErrorKind::CircuitOpen);
    }

    #[tokio::test]
    async fn pool_should_hold_bound_when_requests_concurrent() {
        let (pool, server) = fake_pool(FakeBehavior::BraveOk(sample_results()), 10_000, 10_000, 2);
        let pool = Arc::new(pool);

        let mut handles = Vec::new();
        for _ in 0..6 {
            let pool = Arc::clone(&pool);
            handles.push(tokio::spawn(async move { pool.search("q", 5).await }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert!(
            server.max_concurrent.load(Ordering::SeqCst) <= 2,
            "pool of 2 must never run more than 2 backend searches at once"
        );
    }
}
