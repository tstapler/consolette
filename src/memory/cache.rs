//! `ResponseCache`: SQLite-backed semantic cache for `POST /v1/messages`
//! (issue #24).
//!
//! Agentic coding loops frequently re-send a near-identical request —
//! the same tool-result turn reached twice, a client-side retry, or two
//! sibling agents asking the same question against the same context.
//! When the *shape that actually determines the model's answer* (model,
//! system prompt, tool definitions, and the newest turn) is unchanged from
//! a previous request, replaying that previous response skips an upstream
//! LLM call entirely — zero latency, zero additional spend.
//!
//! Scope (deliberately narrower than a general semantic cache):
//! - Only non-streaming `/v1/messages` requests are considered (the
//!   caller filters this before calling in — see
//!   `crate::entrypoint::messages::post_v1_messages`). Replaying a cached
//!   response as a synthetic SSE stream is a reasonable follow-up, not
//!   required by this feature.
//! - The emulated server-tool loop (`handle_emulated_search`) is not
//!   wired to this cache — that path already executes a bounded,
//!   side-effecting loop against a search backend, which is outside this
//!   feature's "duplicate read-only turn" framing.
//! - Cache hits do not feed `CostTracker`/per-model token counters: no
//!   upstream tokens were actually spent, so counting them would inflate
//!   both. Hits are tracked separately via [`ResponseCache::stats_json`].

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use axum::http::HeaderMap;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cost_metrics::pricing::PricingTable;
use crate::providers::extract_usage;

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS response_cache (
    cache_key          TEXT PRIMARY KEY,
    response_json      TEXT NOT NULL,
    model              TEXT NOT NULL,
    estimated_cost_usd REAL NOT NULL,
    created_at         TEXT NOT NULL,
    expires_at         TEXT NOT NULL
)";

/// Request header a client sets to force a miss for one request, without
/// disabling the cache globally. Deliberately not Anthropic's own
/// `cache_control` field (prompt/context caching) — this is a distinct,
/// proxy-internal concern and must not collide with that wire format.
pub const BYPASS_HEADER: &str = "x-consolette-cache-bypass";

/// Tuning knobs for the `[response_cache]` config table. All optional; a
/// missing table yields the cache disabled (opt-in — a stale replayed
/// answer is a worse failure mode than a missed cache hit).
///
/// **Residual risk, by design, not yet mitigated**: [`compute_cache_key`]
/// has no session/origin binding, and `entrypoint_router`'s CORS policy is
/// wide open (`allow_origin(Any)`) to match this proxy's documented no-auth
/// loopback trust model. Any local process, or any webpage open in the
/// user's browser, can therefore plant a response under a key a *different*
/// legitimate request will later present — and for a coding agent whose
/// system prompt and tool schemas are largely fixed/predictable, that key
/// isn't hard to guess. This is strictly worse than the pre-existing no-auth
/// risk (which only ever affected the forged request's own response): once
/// this cache is enabled, a forged request can corrupt what a *different*
/// session receives. Enable `[response_cache]` only when you trust every
/// process and browser tab that can reach this proxy's loopback port.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseCacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
    /// Tool names whose presence in the request's newest turn forces a
    /// miss (and skips the write on the way back). Replaying a cached
    /// answer that followed a `Bash`/`Edit`/`Write` tool result risks
    /// echoing stale guidance about mutated state; read-only tools (e.g.
    /// `Read`, `Grep`) are safe to cache.
    #[serde(default = "default_mutating_tools")]
    pub mutating_tools: Vec<String>,
}

fn default_ttl_secs() -> u64 {
    900
}

fn default_mutating_tools() -> Vec<String> {
    vec!["Bash".to_string(), "Edit".to_string(), "Write".to_string()]
}

impl Default for ResponseCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: default_ttl_secs(),
            mutating_tools: default_mutating_tools(),
        }
    }
}

/// Request fields that affect generation behavior (not content) and must
/// therefore be part of the cache key — two requests with identical
/// conversational content but, say, different `temperature` are asking
/// for different *behavior*, not the same cached answer.
const GENERATION_PARAM_FIELDS: &[&str] = &[
    "max_tokens",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
];

/// Deterministic cache key: sha256 of the model name, system prompt, tool
/// definitions, generation parameters, and newest message in `body` — the
/// fields that actually determine the model's next turn. Earlier turns are
/// deliberately excluded: two requests that reached the same newest turn
/// via different history are, for caching purposes, asking the same
/// question.
#[must_use]
pub fn compute_cache_key(body: &Value) -> String {
    let model = body.get("model").cloned().unwrap_or(Value::Null);
    let system = body.get("system").cloned().unwrap_or(Value::Null);
    let tools = body.get("tools").cloned().unwrap_or(Value::Null);
    let last_message = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .cloned()
        .unwrap_or(Value::Null);

    let mut hasher = Sha256::new();
    // `serde_json::Value`'s map is a `BTreeMap` (no `preserve_order`
    // feature enabled in this crate), so `.to_string()` serializes object
    // keys in a stable, canonical order regardless of request field order.
    for part in [&model, &system, &tools, &last_message] {
        hasher.update(part.to_string().as_bytes());
        hasher.update(b"\0");
    }
    for field in GENERATION_PARAM_FIELDS {
        let value = body.get(*field).cloned().unwrap_or(Value::Null);
        hasher.update(value.to_string().as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

/// Whether `headers` carry an explicit bypass for this one request.
#[must_use]
pub fn cache_bypass_requested(headers: &HeaderMap) -> bool {
    headers
        .get(BYPASS_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Whether the newest turn in `body` is a `tool_result` for a tool named
/// in `mutating_tools`. Resolves `tool_use_id` → tool name by scanning the
/// whole conversation for the matching `tool_use` block, since
/// `tool_result` blocks carry only the id.
#[must_use]
pub fn body_uses_mutating_tool(body: &Value, mutating_tools: &[String]) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };

    let mut tool_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
                tool_names.insert(id, name);
            }
        }
    }

    let Some(last_content) = messages
        .last()
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return false;
    };

    last_content.iter().any(|block| {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            return false;
        }
        match block.get("tool_use_id").and_then(Value::as_str) {
            Some(id) => match tool_names.get(id) {
                // A resolvable id: bypass only if it names a configured
                // mutating tool.
                Some(name) => mutating_tools.iter().any(|m| m == name),
                // An id with no matching `tool_use` anywhere in the
                // conversation: fail closed. We cannot verify this was a
                // read-only tool, and `/v1/messages` has no auth — an
                // external caller can send an orphaned `tool_result`
                // directly, so this path is reachable, not hypothetical.
                None => true,
            },
            // No `tool_use_id` at all: same fail-closed reasoning.
            None => true,
        }
    })
}

/// SQLite-backed cache of full (non-streaming) Anthropic Messages API
/// responses, keyed by [`compute_cache_key`]. Mirrors
/// `claude_code_session::omission_cache::OmissionCache`'s open/harden
/// pattern — this cache stores full response bodies, which can echo back
/// whatever the request's tool results contained.
pub struct ResponseCache {
    conn: Mutex<Connection>,
    pricing: PricingTable,
    pub config: ResponseCacheConfig,
    hits: AtomicU64,
    misses: AtomicU64,
    bypassed: AtomicU64,
    estimated_savings_usd: Mutex<f64>,
}

impl ResponseCache {
    /// Open (creating if necessary) the sqlite database at `path`, with
    /// the same WAL + permission hardening as `OmissionCache::open`.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory can't be created, the
    /// connection can't be opened, the schema/pragmas can't be applied, or
    /// permissions can't be set.
    pub fn open(path: &Path, config: ResponseCacheConfig) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| {
                    anyhow!(
                        "failed to create response cache directory {}: {error}",
                        parent.display()
                    )
                })?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        anyhow!(
                            "failed to set 0700 permissions on {}: {error}",
                            parent.display()
                        )
                    },
                )?;
            }
        }

        let conn = Connection::open(path).map_err(|error| {
            anyhow!("failed to open response cache {}: {error}", path.display())
        })?;

        conn.execute(CREATE_TABLE_SQL, [])
            .map_err(|error| anyhow!("failed to create response_cache table: {error}"))?;

        let journal_mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|error| anyhow!("failed to set journal_mode=WAL: {error}"))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(anyhow!(
                "expected journal_mode=WAL, sqlite reported {journal_mode}"
            ));
        }

        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| anyhow!("failed to set busy_timeout=5000: {error}"))?;

        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
                anyhow!(
                    "failed to set 0600 permissions on {}: {error}",
                    path.display()
                )
            })?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
            pricing: PricingTable::load_default(),
            config,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bypassed: AtomicU64::new(0),
            estimated_savings_usd: Mutex::new(0.0),
        })
    }

    /// `~/.claude/consolette/response-cache.sqlite` — same directory
    /// convention as `OmissionCache::default_cache_path`.
    #[must_use]
    pub fn default_cache_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".claude")
            .join("consolette")
            .join("response-cache.sqlite")
    }

    /// Returns the key `body`/`headers` should be cached under, or `None`
    /// if this request must not consult or populate the cache: disabled,
    /// an explicit per-request bypass, or the newest turn is a result for
    /// a configured mutating tool. A `None` from a bypass/mutating check
    /// also counts toward `bypassed` in [`Self::stats_json`].
    #[must_use]
    pub fn cacheable_key(&self, body: &Value, headers: &HeaderMap) -> Option<String> {
        if !self.config.enabled {
            return None;
        }
        if cache_bypass_requested(headers)
            || body_uses_mutating_tool(body, &self.config.mutating_tools)
        {
            self.bypassed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(compute_cache_key(body))
    }

    /// Looks up `key`. A row past its TTL is deleted and treated as a
    /// miss rather than returned stale.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        let row = self.select_row(key);

        let Some((response_json, estimated_cost_usd, expires_at)) = row else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map(|expires_at| expires_at < Utc::now())
            .unwrap_or(true);
        if expired {
            // Guarded by the exact `expires_at` this call observed, not a
            // bare key match: a concurrent `put()` between the SELECT above
            // and this DELETE would have written a new row with a *new*
            // `expires_at`, and the guard makes this a no-op against that
            // fresh row instead of deleting it out from under the writer.
            self.delete_if_still_expired(key, &expires_at);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let Ok(value) = serde_json::from_str::<Value>(&response_json) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        self.hits.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut total) = self.estimated_savings_usd.lock() {
            *total += estimated_cost_usd;
        }
        Some(value)
    }

    fn select_row(&self, key: &str) -> Option<(String, f64, String)> {
        let conn = self.conn.lock().ok()?;
        match conn.query_row(
            "SELECT response_json, estimated_cost_usd, expires_at FROM response_cache WHERE cache_key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(error) => {
                tracing::warn!(%error, cache_key = key, "response cache lookup failed");
                None
            }
        }
    }

    fn delete_if_still_expired(&self, key: &str, expected_expires_at: &str) {
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        if let Err(error) = conn.execute(
            "DELETE FROM response_cache WHERE cache_key = ?1 AND expires_at = ?2",
            params![key, expected_expires_at],
        ) {
            tracing::warn!(%error, cache_key = key, "response cache expired-entry cleanup failed");
        }
    }

    /// Stores `response` (a full Anthropic Messages API body) under `key`.
    /// Best-effort: a write failure is dropped, never surfaced to the
    /// caller — a cache write must never turn a successful response into
    /// an error. Opportunistically prunes expired rows on each write so
    /// the table doesn't grow unbounded between restarts.
    pub fn put(&self, key: &str, model: &str, response: &Value) {
        let estimated_cost_usd = extract_usage(response)
            .zip(self.pricing.price_for(model))
            .map(|(usage, price)| {
                (usage.input_tokens as f64) * price.input_usd_per_token
                    + (usage.output_tokens as f64) * price.output_usd_per_token
            })
            .unwrap_or(0.0);

        let Ok(response_json) = serde_json::to_string(response) else {
            return;
        };
        let now = Utc::now();
        let ttl_secs = i64::try_from(self.config.ttl_secs).unwrap_or(i64::MAX);
        let expires_at = now + chrono::Duration::seconds(ttl_secs);

        let Ok(conn) = self.conn.lock() else {
            tracing::warn!(
                cache_key = key,
                "response cache mutex poisoned, dropping write"
            );
            return;
        };
        if let Err(error) = conn.execute(
            "DELETE FROM response_cache WHERE expires_at < ?1",
            params![now.to_rfc3339()],
        ) {
            tracing::warn!(%error, "response cache expired-rows cleanup failed");
        }
        if let Err(error) = conn.execute(
            "INSERT OR REPLACE INTO response_cache \
             (cache_key, response_json, model, estimated_cost_usd, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                key,
                response_json,
                model,
                estimated_cost_usd,
                now.to_rfc3339(),
                expires_at.to_rfc3339()
            ],
        ) {
            tracing::warn!(%error, cache_key = key, "response cache write failed");
        }
    }

    /// Hit-ratio/savings snapshot for `GET /metrics`'s `response_cache`
    /// field, which the dashboard polls directly (no separate counter
    /// store — these are in-memory and reset on restart, matching the
    /// other dashboard counters in `MetricsCollector`).
    #[must_use]
    pub fn stats_json(&self) -> Value {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let bypassed = self.bypassed.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_ratio = if total > 0 {
            hits as f64 / total as f64
        } else {
            0.0
        };
        let estimated_savings_usd = self
            .estimated_savings_usd
            .lock()
            .map(|guard| *guard)
            .unwrap_or(0.0);

        serde_json::json!({
            "enabled": self.config.enabled,
            "hits": hits,
            "misses": misses,
            "bypassed": bypassed,
            "hit_ratio": hit_ratio,
            "estimated_savings_usd": estimated_savings_usd,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn temp_cache(dir: &TempDir, config: ResponseCacheConfig) -> ResponseCache {
        ResponseCache::open(&dir.path().join("response-cache.sqlite"), config).unwrap()
    }

    fn enabled_config() -> ResponseCacheConfig {
        ResponseCacheConfig {
            enabled: true,
            ..ResponseCacheConfig::default()
        }
    }

    #[test]
    fn get_should_return_none_when_key_was_never_written() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());

        assert_eq!(cache.get("missing-key"), None);
    }

    #[test]
    fn put_then_get_should_round_trip_the_same_response_body() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let response = json!({"id": "msg_1", "content": [{"type": "text", "text": "hi"}]});

        cache.put("key-a", "claude-sonnet-5", &response);

        assert_eq!(cache.get("key-a"), Some(response));
    }

    #[test]
    fn get_should_treat_an_expired_entry_as_a_miss_and_delete_it() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(
            &dir,
            ResponseCacheConfig {
                enabled: true,
                ttl_secs: 0,
                ..ResponseCacheConfig::default()
            },
        );
        cache.put("key-a", "claude-sonnet-5", &json!({"id": "msg_1"}));

        // ttl_secs = 0 means expires_at == created_at; any elapsed wall
        // clock time afterward makes the row stale.
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert_eq!(cache.get("key-a"), None);
    }

    #[test]
    fn cacheable_key_should_return_none_when_cache_disabled() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, ResponseCacheConfig::default());
        let body = json!({"model": "claude-sonnet-5", "messages": []});

        assert_eq!(cache.cacheable_key(&body, &HeaderMap::new()), None);
    }

    #[test]
    fn cacheable_key_should_return_none_when_bypass_header_is_true() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({"model": "claude-sonnet-5", "messages": []});
        let mut headers = HeaderMap::new();
        headers.insert(BYPASS_HEADER, "true".parse().unwrap());

        assert_eq!(cache.cacheable_key(&body, &headers), None);
    }

    #[test]
    fn cacheable_key_should_return_none_when_newest_turn_is_a_bash_tool_result() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]}
            ]
        });

        assert_eq!(cache.cacheable_key(&body, &HeaderMap::new()), None);
    }

    #[test]
    fn cacheable_key_should_return_some_when_newest_turn_is_a_read_tool_result() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "file contents"}
                ]}
            ]
        });

        assert!(cache.cacheable_key(&body, &HeaderMap::new()).is_some());
    }

    #[test]
    fn cacheable_key_should_return_none_when_matching_tool_use_is_several_turns_back() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]},
                {"role": "assistant", "content": "noted"},
                {"role": "user", "content": "now do something else"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "done"}
                ]}
            ]
        });

        assert_eq!(cache.cacheable_key(&body, &HeaderMap::new()), None);
    }

    #[test]
    fn cacheable_key_should_return_none_when_one_of_several_parallel_tool_results_is_mutating() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "file contents"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "ran"}
                ]}
            ]
        });

        assert_eq!(cache.cacheable_key(&body, &HeaderMap::new()), None);
    }

    #[test]
    fn cacheable_key_should_return_none_when_tool_result_has_no_matching_tool_use() {
        // Fail closed: an orphaned `tool_use_id` (no matching `tool_use`
        // anywhere in the conversation) can't be verified as a read-only
        // tool, and /v1/messages has no auth — an external caller can send
        // this directly, so it must miss rather than risk a replay.
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_unknown", "content": "ok"}
                ]}
            ]
        });

        assert_eq!(cache.cacheable_key(&body, &HeaderMap::new()), None);
    }

    #[test]
    fn compute_cache_key_should_differ_when_generation_parameters_differ() {
        let base = json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "same question"}]
        });
        let different_max_tokens = json!({
            "model": "m",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "same question"}]
        });
        let different_temperature = json!({
            "model": "m",
            "max_tokens": 100,
            "temperature": 1,
            "messages": [{"role": "user", "content": "same question"}]
        });

        assert_ne!(
            compute_cache_key(&base),
            compute_cache_key(&different_max_tokens)
        );
        assert_ne!(
            compute_cache_key(&base),
            compute_cache_key(&different_temperature)
        );
    }

    #[test]
    fn put_should_default_estimated_cost_to_zero_when_model_is_unpriced() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let response = json!({"usage": {"input_tokens": 1000, "output_tokens": 500}});

        cache.put("key-a", "some-totally-unknown-model-id", &response);
        let _ = cache.get("key-a");

        let stats = cache.stats_json();
        assert_eq!(stats["hits"], 1);
        assert_eq!(stats["estimated_savings_usd"], 0.0);
    }

    #[test]
    fn compute_cache_key_should_be_identical_for_requests_differing_only_in_earlier_turns() {
        let base = json!({
            "model": "claude-sonnet-5",
            "system": "you are helpful",
            "tools": [],
            "messages": [
                {"role": "user", "content": "turn one"},
                {"role": "assistant", "content": "turn one reply"},
                {"role": "user", "content": "same newest question"},
            ]
        });
        let other_history = json!({
            "model": "claude-sonnet-5",
            "system": "you are helpful",
            "tools": [],
            "messages": [
                {"role": "user", "content": "a completely different earlier turn"},
                {"role": "user", "content": "same newest question"},
            ]
        });

        assert_eq!(compute_cache_key(&base), compute_cache_key(&other_history));
    }

    #[test]
    fn compute_cache_key_should_differ_when_newest_turn_differs() {
        let a = json!({"model": "m", "messages": [{"role": "user", "content": "one"}]});
        let b = json!({"model": "m", "messages": [{"role": "user", "content": "two"}]});

        assert_ne!(compute_cache_key(&a), compute_cache_key(&b));
    }

    #[test]
    fn stats_json_should_report_zero_hit_ratio_before_any_lookups() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());

        let stats = cache.stats_json();
        assert_eq!(stats["hits"], 0);
        assert_eq!(stats["misses"], 0);
        assert_eq!(stats["hit_ratio"], 0.0);
    }

    #[test]
    fn stats_json_should_count_a_hit_and_accumulate_its_estimated_savings() {
        let dir = TempDir::new().unwrap();
        let cache = temp_cache(&dir, enabled_config());
        let response = json!({
            "usage": {"input_tokens": 1000, "output_tokens": 500}
        });
        cache.put("key-a", "claude-sonnet-4-5-20250929", &response);

        let _ = cache.get("key-a");

        let stats = cache.stats_json();
        assert_eq!(stats["hits"], 1);
        assert_eq!(stats["misses"], 0);
        assert_eq!(stats["hit_ratio"], 1.0);
        assert!(stats["estimated_savings_usd"].as_f64().unwrap() > 0.0);
    }
}
