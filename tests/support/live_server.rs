//! Real consolette server on an OS-assigned loopback port, fronting one
//! Anthropic-kind upstream, plus a raw-socket SSE/HTTP client.
//!
//! Raw `TcpStream`s (not reqwest) so a test can abort a request by dropping
//! the socket, which is what a disconnecting client does.

use std::time::{Duration, Instant};

use consolette::entrypoint::{entrypoint_router, EntrypointState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

pub struct LiveServer {
    pub addr: std::net::SocketAddr,
    pub state: EntrypointState,
    task: JoinHandle<()>,
    _config_dir: tempfile::TempDir,
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LiveServer {
    /// Serves one route `default` over a single upstream at `upstream_base_url`.
    pub async fn start(upstream_base_url: &str) -> Self {
        Self::start_config(&format!(
            r#"
{}
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "fake"
"#,
            Self::anthropic_upstream("fake", upstream_base_url)
        ))
        .await
    }

    /// `[[upstreams]]` TOML block for an Anthropic-kind upstream with an inline key.
    pub fn anthropic_upstream(name: &str, base_url: &str) -> String {
        format!(
            r#"
[[upstreams]]
name = "{name}"
kind = "anthropic"
base_url = "{base_url}"

[upstreams.auth]
type = "apikey"

[upstreams.auth.key]
source = "inline"
value = "sk-test"
"#
        )
    }

    /// `[[upstreams]]` TOML block for an OpenAI-kind upstream with an inline bearer token.
    pub fn openai_upstream(name: &str, base_url: &str) -> String {
        format!(
            r#"
[[upstreams]]
name = "{name}"
kind = "openai"
base_url = "{base_url}"

[upstreams.auth]
type = "bearer"

[upstreams.auth.token]
source = "inline"
value = "sk-test"
"#
        )
    }

    /// Serves the given `conf.d` TOML (upstreams, routes, extra tables); the
    /// harness prepends `local_discovery = false` and `health_check_interval_secs = 0`.
    pub async fn start_config(toml_body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let conf_d = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        std::fs::write(
            conf_d.join("00-live.toml"),
            format!("local_discovery = false\nhealth_check_interval_secs = 0\n{toml_body}"),
        )
        .unwrap();
        let config = consolette::config::load(dir.path()).unwrap();
        let state = EntrypointState::build(&config, dir.path()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = entrypoint_router(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            state,
            task,
            _config_dir: dir,
        }
    }

    /// Directory holding `conf.d`, for tests that need to drop files beside the config.
    pub fn config_dir(&self) -> &std::path::Path {
        self._config_dir.path()
    }

    /// Opens the dashboard SSE stream and returns once response headers arrive
    /// (so the subscription exists before any request is sent).
    pub async fn open_sse(&self) -> SseClient {
        let mut sock = TcpStream::connect(self.addr).await.unwrap();
        sock.write_all(
            b"GET /v1/dashboard/events HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();
        let mut client = SseClient {
            sock,
            buf: String::new(),
        };
        client
            .read_until(Duration::from_secs(5), |b| b.contains("\r\n\r\n"))
            .await;
        client
    }

    /// Sends `POST /v1/messages` and returns the open socket without reading.
    pub async fn post_messages(&self, body: &serde_json::Value) -> TcpStream {
        let payload = body.to_string();
        let mut sock = TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        sock
    }

    /// `GET /metrics` as JSON over a fresh connection.
    pub async fn metrics(&self) -> serde_json::Value {
        self.request_json("GET", "/metrics").await
    }

    /// Bodyless `method path` over a fresh `Connection: close` socket; the body parsed as JSON.
    pub async fn request_json(&self, method: &str, path: &str) -> serde_json::Value {
        let mut sock = TcpStream::connect(self.addr).await.unwrap();
        sock.write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut raw = String::new();
        sock.read_to_string(&mut raw).await.unwrap();
        let body = raw.split_once("\r\n\r\n").unwrap().1;
        // Body may be chunked; take the outermost JSON object.
        let start = body.find('{').unwrap();
        let end = body.rfind('}').unwrap();
        serde_json::from_str(&body[start..=end]).unwrap()
    }
}

/// Reads a response off `sock` until the server closes or `within` elapses;
/// returns `(status, body)`. Chunked bodies are returned undecoded-but-JSON-extractable via
/// [`response_json`].
pub async fn read_response(sock: &mut TcpStream, within: Duration) -> (u16, String) {
    let deadline = Instant::now() + within;
    let mut raw = String::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((head, body)) = raw.split_once("\r\n\r\n") {
            let len = head.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            });
            let chunked_done = head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
                && body.ends_with("0\r\n\r\n");
            if len.is_some_and(|n| body.len() >= n) || chunked_done {
                break;
            }
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("response timed out; got so far: {raw}"));
        match tokio::time::timeout(left, sock.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => raw.push_str(&String::from_utf8_lossy(&chunk[..n])),
            Ok(_) => break,
            Err(_) => panic!("response timed out; got so far: {raw}"),
        }
    }
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in: {raw}"));
    (status, body.to_string())
}

/// Outermost JSON object in a (possibly chunk-framed) body.
pub fn response_json(body: &str) -> serde_json::Value {
    let start = body.find('{').unwrap();
    let end = body.rfind('}').unwrap();
    serde_json::from_str(&body[start..=end]).unwrap()
}

pub struct SseClient {
    sock: TcpStream,
    buf: String,
}

impl SseClient {
    async fn read_until(&mut self, within: Duration, done: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + within;
        let mut chunk = [0u8; 4096];
        while !done(&self.buf) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match tokio::time::timeout(left, self.sock.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                _ => return false,
            }
        }
        true
    }

    /// Reads until `needle` appears (or `within` elapses) and returns every byte received so far.
    pub async fn collect_until(&mut self, within: Duration, needle: &str) -> String {
        let _ = self.read_until(within, |b| b.contains(needle)).await;
        self.buf.clone()
    }

    /// Waits up to `within` for the first `RequestTrace` payload and returns it,
    /// then keeps listening `settle` longer to collect any extra traces.
    pub async fn request_traces(
        &mut self,
        within: Duration,
        settle: Duration,
    ) -> Vec<serde_json::Value> {
        let _ = self
            .read_until(within, |b| {
                b.contains("\"type\":\"RequestTrace\"") || b.contains("\"type\": \"RequestTrace\"")
            })
            .await;
        let _ = self.read_until(settle, |_| false).await;
        self.buf
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
            .filter(|v| v["type"] == "RequestTrace")
            .collect()
    }
}
