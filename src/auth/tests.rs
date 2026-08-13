#![allow(clippy::unwrap_used)]

use std::sync::{Mutex, OnceLock};

use http::HeaderMap;

use super::*;
use crate::config::schema::{AuthMethod, SecretRef};

/// Serializes tests that mutate process-global env vars — separate lock
/// instance from `config::tests::env_lock()` since these are different
/// modules touching disjoint var names, but the pattern is kept consistent.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Dummy request-context args shared by tests that don't exercise `exec`
/// dispatch — bearer/apikey ignore them entirely.
const UPSTREAM: &str = "test-upstream";
const METHOD: &str = "POST";
const URL: &str = "https://example.invalid/v1/messages";

#[tokio::test]
async fn bearer_applies_authorization_header() {
    let method = AuthMethod::Bearer {
        token: SecretRef::Inline {
            value: "sk-test".to_string(),
        },
    };
    let mut headers = HeaderMap::new();
    let cache = exec::ExecCredentialCache::new();

    method
        .apply(
            &mut headers,
            &SystemSecretResolver,
            UPSTREAM,
            METHOD,
            URL,
            &cache,
        )
        .await
        .unwrap();

    assert_eq!(headers.get("Authorization").unwrap(), "Bearer sk-test");
}

#[tokio::test]
async fn apikey_applies_default_header() {
    let method = AuthMethod::Apikey {
        key: SecretRef::Inline {
            value: "abc123".to_string(),
        },
        header: "x-api-key".to_string(),
    };
    let mut headers = HeaderMap::new();
    let cache = exec::ExecCredentialCache::new();

    method
        .apply(
            &mut headers,
            &SystemSecretResolver,
            UPSTREAM,
            METHOD,
            URL,
            &cache,
        )
        .await
        .unwrap();

    assert_eq!(headers.get("x-api-key").unwrap(), "abc123");
}

#[tokio::test]
async fn apikey_applies_custom_header_name() {
    let method = AuthMethod::Apikey {
        key: SecretRef::Inline {
            value: "abc123".to_string(),
        },
        header: "x-custom-key".to_string(),
    };
    let mut headers = HeaderMap::new();
    let cache = exec::ExecCredentialCache::new();

    method
        .apply(
            &mut headers,
            &SystemSecretResolver,
            UPSTREAM,
            METHOD,
            URL,
            &cache,
        )
        .await
        .unwrap();

    assert_eq!(headers.get("x-custom-key").unwrap(), "abc123");
    assert!(headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn exec_apply_dispatches_to_helper() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "echo-helper.sh",
        r#"#!/bin/sh
cat >/dev/null
echo '{"headers":{"Authorization":"Bearer helper-token"}}'
"#,
    );
    let method = AuthMethod::Exec {
        command: helper.to_string_lossy().to_string(),
        args: vec![],
        cache_ttl_secs: 300,
        timeout_secs: 5,
    };
    let mut headers = HeaderMap::new();
    let cache = exec::ExecCredentialCache::new();

    method
        .apply(
            &mut headers,
            &SystemSecretResolver,
            UPSTREAM,
            METHOD,
            URL,
            &cache,
        )
        .await
        .unwrap();

    assert_eq!(headers.get("Authorization").unwrap(), "Bearer helper-token");
}

#[test]
fn env_secret_resolves_from_process_env() {
    let _guard = env_lock().lock().unwrap();
    std::env::set_var("AUTH_TEST_TOKEN", "env-value");

    let result = SystemSecretResolver.resolve(&SecretRef::Env {
        var: "AUTH_TEST_TOKEN".to_string(),
    });

    std::env::remove_var("AUTH_TEST_TOKEN");

    assert_eq!(result.unwrap(), "env-value");
}

#[test]
fn env_secret_missing_var_errors() {
    let _guard = env_lock().lock().unwrap();
    std::env::remove_var("AUTH_TEST_MISSING_VAR");

    let result = SystemSecretResolver.resolve(&SecretRef::Env {
        var: "AUTH_TEST_MISSING_VAR".to_string(),
    });

    assert!(matches!(result, Err(AuthError::Resolve(_))));
}

#[test]
fn inline_secret_resolves_directly() {
    let result = SystemSecretResolver.resolve(&SecretRef::Inline {
        value: "plain".to_string(),
    });

    assert_eq!(result.unwrap(), "plain");
}

#[test]
fn keychain_command_builds_expected_argv() {
    // Never executed — the real `security` CLI would mutate/prompt against
    // the developer's actual Keychain. This only asserts the argv shape.
    let cmd = keychain_command("consolette-test-item");

    assert_eq!(cmd.get_program(), "security");
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(
        args,
        vec!["find-generic-password", "-w", "-s", "consolette-test-item"]
    );
}

/// A resolver stub for testing error propagation without touching real
/// secrets.
struct FailingResolver;

impl SecretResolver for FailingResolver {
    fn resolve(&self, _secret: &SecretRef) -> Result<String, AuthError> {
        Err(AuthError::Resolve("boom".to_string()))
    }
}

#[tokio::test]
async fn resolver_error_propagates_through_apply() {
    let method = AuthMethod::Bearer {
        token: SecretRef::Inline {
            value: "unused".to_string(),
        },
    };
    let mut headers = HeaderMap::new();
    let cache = exec::ExecCredentialCache::new();

    let result = method
        .apply(
            &mut headers,
            &FailingResolver,
            UPSTREAM,
            METHOD,
            URL,
            &cache,
        )
        .await;

    assert!(matches!(result, Err(AuthError::Resolve(_))));
}

/// Writes an executable shell script into `dir` and returns its path — used
/// by exec-dispatch tests to build fake credential helpers without shelling
/// out to a real one.
fn write_helper(dir: &tempfile::TempDir, name: &str, script: &str) -> std::path::PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
