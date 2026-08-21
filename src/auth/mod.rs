//! ADR-002: pluggable per-upstream auth — resolving a `SecretRef` to a value
//! and applying an `AuthMethod` to outbound request headers.
//!
//! `exec` (ADR-007 §2) dispatches through the [`exec`] submodule: spawning
//! the configured helper, sending a JSON request context on stdin, and
//! merging the JSON header map it returns on stdout. Plugin discovery
//! (`plugins.d/*/`, `plugin.toml`) is a separate, not-yet-implemented
//! ADR-007 story — `command` only resolves via an explicit path or `PATH`.

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::config::schema::{AuthMethod, SecretRef};

pub mod exec;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("failed to resolve secret: {0}")]
    Resolve(String),
    #[error("{0:?} is not a valid header name")]
    InvalidHeaderName(String),
    #[error("resolved value for header {0:?} is not a valid header value")]
    InvalidHeaderValue(String),
    #[error("{0}")]
    NotImplemented(&'static str),
    #[error("exec credential helper failed: {0}")]
    Exec(String),
}

/// Resolves a `SecretRef` to its plaintext value. A trait (rather than a free
/// function) so tests can substitute a fake resolver instead of touching the
/// real environment or Keychain.
pub trait SecretResolver {
    /// # Errors
    ///
    /// Returns [`AuthError`] if the secret can't be resolved (missing env
    /// var, Keychain lookup failure, etc).
    fn resolve(&self, secret: &SecretRef) -> Result<String, AuthError>;
}

/// The real resolver: `Inline` returns its value directly, `Env` reads the
/// process environment, `Keychain` shells out to `security
/// find-generic-password` (macOS-only, matches plan.md Task 2.1.2 — no
/// keychain crate dependency needed for a single read-only lookup).
pub struct SystemSecretResolver;

impl SecretResolver for SystemSecretResolver {
    fn resolve(&self, secret: &SecretRef) -> Result<String, AuthError> {
        match secret {
            SecretRef::Inline { value } => Ok(value.clone()),
            SecretRef::Env { var } => std::env::var(var)
                .map_err(|_| AuthError::Resolve(format!("env var {var} is not set"))),
            SecretRef::Keychain { item } => resolve_keychain(item),
        }
    }
}

/// Split out so the argv it builds is independently testable without
/// actually invoking `security` (see `tests::keychain_command_builds_expected_argv`).
fn keychain_command(item: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("security");
    cmd.args(["find-generic-password", "-w", "-s", item]);
    cmd
}

fn resolve_keychain(item: &str) -> Result<String, AuthError> {
    let output = keychain_command(item)
        .output()
        .map_err(|e| AuthError::Resolve(format!("failed to run `security`: {e}")))?;
    if !output.status.success() {
        return Err(AuthError::Resolve(format!(
            "security find-generic-password -s {item} failed (exit {:?})",
            output.status.code()
        )));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim_end_matches('\n').to_string())
        .map_err(|_| AuthError::Resolve("keychain value was not valid UTF-8".to_string()))
}

/// Adds the trait/inherent-impl-style `apply` method to `AuthMethod`. Kept as
/// an extension rather than living in `config::schema` — the schema module
/// stays serde-only; request-header behavior lives here (FR-2.2, ADR-002
/// Task 2.1.1).
#[async_trait::async_trait(?Send)]
pub trait AuthMethodExt {
    async fn apply(
        &self,
        headers: &mut HeaderMap,
        resolver: &dyn SecretResolver,
        upstream: &str,
        method: &str,
        url: &str,
        exec_cache: &exec::ExecCredentialCache,
    ) -> Result<(), AuthError>;
}

#[async_trait::async_trait(?Send)]
impl AuthMethodExt for AuthMethod {
    async fn apply(
        &self,
        headers: &mut HeaderMap,
        resolver: &dyn SecretResolver,
        upstream: &str,
        method: &str,
        url: &str,
        exec_cache: &exec::ExecCredentialCache,
    ) -> Result<(), AuthError> {
        match self {
            AuthMethod::Bearer { token } => {
                let value = resolver.resolve(token)?;
                insert_header(headers, "Authorization", &format!("Bearer {value}"))
            }
            AuthMethod::Apikey { key, header } => {
                let value = resolver.resolve(key)?;
                insert_header(headers, header, &value)
            }
            AuthMethod::Exec {
                command,
                args,
                cache_ttl_secs,
                timeout_secs,
            } => {
                let helper_headers = exec_cache
                    .get_or_run(
                        upstream,
                        command,
                        args,
                        std::time::Duration::from_secs(*cache_ttl_secs),
                        std::time::Duration::from_secs(*timeout_secs),
                        method,
                        url,
                    )
                    .await?;
                for (name, value) in &helper_headers {
                    headers.insert(name.clone(), value.clone());
                }
                Ok(())
            }
        }
    }
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), AuthError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| AuthError::InvalidHeaderName(name.to_string()))?;
    let header_value = HeaderValue::from_str(value)
        .map_err(|_| AuthError::InvalidHeaderValue(name.to_string()))?;
    headers.insert(header_name, header_value);
    Ok(())
}

#[cfg(test)]
mod tests;
