//! ADR-007 §2: the `exec` credential-helper protocol. Resolving
//! `auth = { type = "exec", ... }` means spawning `command`, sending a
//! one-line JSON request context on stdin, and reading a one-line JSON
//! header map back on stdout — cached per `(upstream, command, args)` so a
//! subprocess isn't spawned on every request.
//!
//! Plugin discovery (`plugins.d/*/`, `plugin.toml`, `bin/` resolution ahead
//! of `PATH`) is a separate, not-yet-implemented ADR-007 story — `command`
//! here only resolves via an explicit path or `PATH`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::AuthError;

/// The stdin request context (ADR-007 §2): one JSON line, no trailing
/// newline in the value itself — `run_helper` appends it when writing.
#[derive(Serialize)]
struct RequestContext<'a> {
    upstream: &'a str,
    method: &'a str,
    url: &'a str,
}

/// The stdout response (ADR-007 §2). `cache_ttl_secs` optionally overrides
/// the configured TTL for this result.
#[derive(Deserialize)]
struct HelperResponse {
    headers: HashMap<String, String>,
    #[serde(default)]
    cache_ttl_secs: Option<u64>,
}

struct CacheEntry {
    headers: HeaderMap,
    expires_at: Instant,
}

/// Caches successful helper results per `(upstream, command, args)` (ADR-007
/// §2) so a subprocess isn't spawned on every request. `clear()` should be
/// called on SIGHUP/reload — not yet wired up, since there's no reload
/// signal handler in this rebuild yet.
#[derive(Default)]
pub struct ExecCredentialCache {
    entries: DashMap<u64, CacheEntry>,
}

impl ExecCredentialCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&self) {
        self.entries.clear();
    }

    fn cache_key(upstream: &str, command: &str, args: &[String]) -> u64 {
        let mut hasher = DefaultHasher::new();
        upstream.hash(&mut hasher);
        command.hash(&mut hasher);
        args.hash(&mut hasher);
        hasher.finish()
    }

    /// Returns cached headers if fresh, else runs the helper and caches the
    /// result under its (possibly helper-overridden) TTL.
    ///
    /// One parameter per piece of ADR-007 §2 request/cache context — splitting
    /// them into a struct would just move the same 7 fields somewhere else.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_or_run(
        &self,
        upstream: &str,
        command: &str,
        args: &[String],
        default_ttl: Duration,
        timeout: Duration,
        method: &str,
        url: &str,
    ) -> Result<HeaderMap, AuthError> {
        let key = Self::cache_key(upstream, command, args);
        if let Some(entry) = self.entries.get(&key) {
            if entry.expires_at > Instant::now() {
                return Ok(entry.headers.clone());
            }
        }

        let (headers, ttl_override) =
            run_helper(upstream, command, args, timeout, method, url).await?;
        let ttl = ttl_override.map_or(default_ttl, Duration::from_secs);
        self.entries.insert(
            key,
            CacheEntry {
                headers: headers.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(headers)
    }
}

/// Resolves `command` to a path: as given if it contains a `/`, else the
/// first `PATH` entry that has it. No plugin-`bin/` lookup yet — see the
/// module doc comment.
fn resolve_command(command: &str) -> Result<PathBuf, AuthError> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .find(|p| p.is_file())
        })
        .ok_or_else(|| AuthError::Exec(format!("command {command:?} not found on PATH")))
}

/// ADR-007 §5: the helper binary must be owned by the current user and not
/// world-writable. The ADR frames this as a startup check; v1 has no
/// plugin-discovery startup phase yet, so it runs per-dispatch instead —
/// same requirement, checked at the point the helper is actually resolved.
fn check_permissions(path: &Path) -> Result<(), AuthError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| AuthError::Exec(format!("cannot stat {}: {e}", path.display())))?;

    // SAFETY: geteuid() takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(AuthError::Exec(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    if meta.permissions().mode() & 0o002 != 0 {
        return Err(AuthError::Exec(format!(
            "{} is world-writable",
            path.display()
        )));
    }
    Ok(())
}

/// Spawns `command`, writes the request context as one JSON line on stdin,
/// and parses one JSON response line from stdout. Any failure (spawn error,
/// non-zero exit, timeout, unparseable stdout) is `AuthError::Exec` — ADR-007
/// §4 treats all of these as "upstream unavailable" with no separate routing
/// path, so the caller doesn't need to distinguish them further. Helper
/// stdout/stderr content is never included in error text or logs (ADR-007
/// §6): only the fact and shape of a failure is reported.
async fn run_helper(
    upstream: &str,
    command: &str,
    args: &[String],
    timeout: Duration,
    method: &str,
    url: &str,
) -> Result<(HeaderMap, Option<u64>), AuthError> {
    let resolved = resolve_command(command)?;
    check_permissions(&resolved)?;

    let mut line = serde_json::to_string(&RequestContext {
        upstream,
        method,
        url,
    })
    .map_err(|e| AuthError::Exec(format!("failed to encode request context: {e}")))?;
    line.push('\n');

    let mut child = tokio::process::Command::new(&resolved)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AuthError::Exec(format!("failed to spawn {command}: {e}")))?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AuthError::Exec(format!("{command}: no stdin handle")))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AuthError::Exec(format!("failed to write to {command} stdin: {e}")))?;
    }

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| AuthError::Exec(format!("{command} timed out after {timeout:?}")))?
        .map_err(|e| AuthError::Exec(format!("failed to wait on {command}: {e}")))?;

    if !output.status.success() {
        tracing::warn!(
            upstream,
            command,
            exit_code = output.status.code(),
            "exec credential helper exited non-zero (output redacted)"
        );
        return Err(AuthError::Exec(format!(
            "{command} exited with status {:?}",
            output.status.code()
        )));
    }

    let response: HelperResponse = serde_json::from_slice(&output.stdout).map_err(|_| {
        tracing::warn!(
            upstream,
            command,
            "exec credential helper produced unparseable stdout (redacted)"
        );
        AuthError::Exec(format!("{command} produced unparseable stdout"))
    })?;

    let mut headers = HeaderMap::new();
    for (name, value) in &response.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            AuthError::Exec(format!("helper returned invalid header name {name:?}"))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            AuthError::Exec(format!("helper returned invalid header value for {name:?}"))
        })?;
        headers.insert(header_name, header_value);
    }

    Ok((headers, response.cache_ttl_secs))
}

#[cfg(test)]
mod tests;
