#![allow(clippy::unwrap_used)]

use std::fs;
use std::sync::{Mutex, OnceLock};

use tempfile::TempDir;

use super::schema::{AuthMethod, Config, RateLimit, SecretRef, Strategy, UpstreamKind};
use super::validate::validate_references;
use super::{load, ConfigError};

/// Legacy-env tests mutate process-global env vars; serialize them so they
/// don't race other tests in this module.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn conf_d(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("conf.d");
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(dir: &std::path::Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap();
}

#[test]
fn empty_conf_d_yields_working_defaults() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    conf_d(&dir);

    let config = load(dir.path()).unwrap();

    assert_eq!(config.port, 47_000);
    assert_eq!(config.upstreams.len(), 2);
    assert!(config.upstreams.iter().any(|u| u.name == "anthropic"));
    assert!(config.upstreams.iter().any(|u| u.name == "bedrock"));
    assert_eq!(config.routes.len(), 1);
    assert_eq!(config.routes[0].name, "default");
}

#[test]
fn layering_precedence_order() {
    let _guard = env_lock().lock().unwrap();
    // conf.d files are merged in sorted filename order — 20 overrides 10,
    // and both override the built-in default (47000).
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(&conf, "20-port.toml", "port = 9001\n");
    write(&conf, "10-port.toml", "port = 9000\n");

    let config = load(dir.path()).unwrap();

    assert_eq!(config.port, 9001);
}

#[test]
fn env_overlay_wins_over_conf_d() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(&conf, "10-port.toml", "port = 9000\n");

    std::env::set_var("CONSOLETTE_PORT", "9500");
    let result = load(dir.path());
    std::env::remove_var("CONSOLETTE_PORT");

    assert_eq!(result.unwrap().port, 9500);
}

#[test]
fn deny_unknown_field_rejected() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(&conf, "10-bad.toml", "port = 9000\nbogus_field = true\n");

    let result = load(dir.path());

    assert!(matches!(result, Err(ConfigError::Figment(_))));
}

#[test]
fn unsupported_internal_auth_variant_rejected_in_core_schema() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-upstreams.toml",
        r#"
[[upstreams]]
name = "internal"
kind = "anthropic"

[upstreams.auth]
type = "internal_native"
"#,
    );

    let result = load(dir.path());

    assert!(matches!(result, Err(ConfigError::Figment(_))));
}

#[test]
fn exec_variant_accepted_in_core_schema() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-upstreams.toml",
        r#"
[[upstreams]]
name = "internal"
kind = "anthropic"

[upstreams.auth]
type = "exec"
command = "consolette-internal-auth-helper"
args = ["--project", "abc"]

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "internal"
"#,
    );

    let config = load(dir.path()).unwrap();

    let internal = config
        .upstreams
        .iter()
        .find(|u| u.name == "internal")
        .unwrap();
    match &internal.auth {
        Some(AuthMethod::Exec {
            command,
            args,
            cache_ttl_secs,
            timeout_secs,
        }) => {
            assert_eq!(command, "consolette-internal-auth-helper");
            assert_eq!(args, &vec!["--project".to_string(), "abc".to_string()]);
            assert_eq!(*cache_ttl_secs, 300);
            assert_eq!(*timeout_secs, 10);
        }
        other => panic!("expected Exec auth, got {other:?}"),
    }
}

#[test]
fn bedrock_options_reject_unknown_field() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-upstreams.toml",
        r#"
[[upstreams]]
name = "bedrock"
kind = "bedrock"
aws_region = "us-east-1"
typo_field = "oops"
"#,
    );

    let result = load(dir.path());

    assert!(matches!(result, Err(ConfigError::Figment(_))));
}

#[test]
fn unknown_route_upstream_reference_fails_validation() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-routes.toml",
        r#"
[[routes]]
name = "broken"
strategy = "fallback"

[[routes.upstreams]]
name = "does-not-exist"
"#,
    );

    let result = load(dir.path());

    match result {
        Err(ConfigError::UnknownUpstreamReference { upstream, .. }) => {
            assert_eq!(upstream, "does-not-exist");
        }
        other => panic!("expected UnknownUpstreamReference, got {other:?}"),
    }
}

#[test]
fn unknown_ratelimit_upstream_reference_fails_validation() {
    let mut config = Config::default();
    config
        .ratelimit
        .upstreams
        .insert("nonexistent".to_string(), RateLimit::default());

    let result = validate_references(&config);

    assert!(matches!(
        result,
        Err(ConfigError::UnknownUpstreamReference { .. })
    ));
}

/// Task 1.5.3: each of the 12 legacy env names, set **in isolation** with an
/// empty conf.d, must produce the expected field on the built `Config`. One
/// closure per var keeps failures attributable to a single legacy name
/// instead of one combined assertion block.
#[test]
#[allow(clippy::too_many_lines)]
fn legacy_env_var_matrix() {
    #[allow(clippy::type_complexity)]
    let cases: &[(&str, &str, &dyn Fn(&Config))] = &[
        ("PROXY_PORT", "9999", &|c: &Config| {
            assert_eq!(c.port, 9999, "PROXY_PORT -> port");
        }),
        ("COOLDOWN_SECONDS", "42", &|c: &Config| {
            assert_eq!(
                c.cooldown_seconds, 42,
                "COOLDOWN_SECONDS -> cooldown_seconds"
            );
        }),
        ("REQUEST_TIMEOUT", "17", &|c: &Config| {
            assert_eq!(c.request_timeout, 17, "REQUEST_TIMEOUT -> request_timeout");
        }),
        ("BEDROCK_MAX_RETRIES", "5", &|c: &Config| {
            let bedrock = c.upstreams.iter().find(|u| u.name == "bedrock").unwrap();
            match &bedrock.kind {
                UpstreamKind::Bedrock { max_retries, .. } => {
                    assert_eq!(
                        *max_retries,
                        Some(5),
                        "BEDROCK_MAX_RETRIES -> bedrock.max_retries"
                    );
                }
                other => panic!("expected Bedrock upstream, got {other:?}"),
            }
        }),
        ("STAPLER_COMPRESS", "false", &|c: &Config| {
            assert!(!c.compress, "STAPLER_COMPRESS -> compress");
        }),
        ("COMPRESS_FLOOR_BYTES", "2048", &|c: &Config| {
            assert_eq!(
                c.compress_floor_bytes, 2048,
                "COMPRESS_FLOOR_BYTES -> compress_floor_bytes"
            );
        }),
        ("CACHE_ALIGNER", "true", &|c: &Config| {
            assert!(c.cache_aligner, "CACHE_ALIGNER -> cache_aligner");
        }),
        ("VERBOSITY_LEVEL", "3", &|c: &Config| {
            assert_eq!(c.verbosity_level, 3, "VERBOSITY_LEVEL -> verbosity_level");
        }),
        ("MEMORY_MAX_ENTRIES", "50", &|c: &Config| {
            assert_eq!(
                c.memory_max_entries, 50,
                "MEMORY_MAX_ENTRIES -> memory_max_entries"
            );
        }),
        ("AWS_PROFILE", "test-profile", &|c: &Config| {
            let bedrock = c.upstreams.iter().find(|u| u.name == "bedrock").unwrap();
            match &bedrock.kind {
                UpstreamKind::Bedrock { aws_profile, .. } => {
                    assert_eq!(
                        aws_profile.as_deref(),
                        Some("test-profile"),
                        "AWS_PROFILE -> bedrock.aws_profile"
                    );
                }
                other => panic!("expected Bedrock upstream, got {other:?}"),
            }
        }),
        ("AWS_REGION", "us-west-2", &|c: &Config| {
            let bedrock = c.upstreams.iter().find(|u| u.name == "bedrock").unwrap();
            match &bedrock.kind {
                UpstreamKind::Bedrock { aws_region, .. } => {
                    assert_eq!(
                        aws_region.as_deref(),
                        Some("us-west-2"),
                        "AWS_REGION -> bedrock.aws_region"
                    );
                }
                other => panic!("expected Bedrock upstream, got {other:?}"),
            }
        }),
        ("CLAUDE_CODE_OAUTH_TOKEN", "sk-test-token", &|c: &Config| {
            let anthropic = c.upstreams.iter().find(|u| u.name == "anthropic").unwrap();
            assert_eq!(
                anthropic.auth,
                Some(AuthMethod::Bearer {
                    token: SecretRef::Env {
                        var: "CLAUDE_CODE_OAUTH_TOKEN".to_string()
                    }
                }),
                "CLAUDE_CODE_OAUTH_TOKEN -> anthropic.auth"
            );
        }),
    ];
    assert_eq!(
        cases.len(),
        12,
        "all 12 legacy vars from Task 1.5.2 must be covered"
    );

    for (name, value, assert_field) in cases {
        let _guard = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        conf_d(&dir);

        std::env::set_var(name, value);
        let result = load(dir.path());
        std::env::remove_var(name);

        let config = result.unwrap_or_else(|e| panic!("{name}: load() failed: {e}"));
        assert_field(&config);
    }
}

#[test]
fn conf_d_aws_region_wins_over_legacy_env() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-upstreams.toml",
        r#"
[[upstreams]]
name = "anthropic"
kind = "anthropic"

[[upstreams]]
name = "bedrock"
kind = "bedrock"
aws_region = "eu-west-1"

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"

[[routes.upstreams]]
name = "bedrock"
"#,
    );

    std::env::set_var("AWS_REGION", "us-west-2");
    let result = load(dir.path());
    std::env::remove_var("AWS_REGION");

    let config = result.unwrap();
    let bedrock = config
        .upstreams
        .iter()
        .find(|u| u.name == "bedrock")
        .unwrap();
    match &bedrock.kind {
        UpstreamKind::Bedrock { aws_region, .. } => {
            assert_eq!(aws_region.as_deref(), Some("eu-west-1"));
        }
        other => panic!("expected Bedrock upstream, got {other:?}"),
    }
}

#[test]
fn secret_ref_debug_redacts_inline_value() {
    let inline = SecretRef::Inline {
        value: "super-secret".to_string(),
    };
    let debug = format!("{inline:?}");

    assert!(!debug.contains("super-secret"));

    let env = SecretRef::Env {
        var: "SOME_VAR".to_string(),
    };
    assert_eq!(format!("{env:?}"), "Env(SOME_VAR)");
}

#[test]
fn secret_ref_serialize_redacts_inline_value() {
    // Debug redaction alone isn't enough — any future JSON dump of Config
    // (e.g. a "show effective config" command) would otherwise round-trip
    // the plaintext secret straight through a derived Serialize impl.
    let inline = SecretRef::Inline {
        value: "super-secret".to_string(),
    };
    let json = serde_json::to_string(&inline).unwrap();

    assert!(!json.contains("super-secret"));
    assert!(json.contains("<redacted>"));

    let env = SecretRef::Env {
        var: "SOME_VAR".to_string(),
    };
    let json = serde_json::to_string(&env).unwrap();
    assert!(json.contains("SOME_VAR"));
}

#[test]
fn weighted_strategy_round_trips() {
    let _guard = env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let conf = conf_d(&dir);
    write(
        &conf,
        "10-upstreams.toml",
        r#"
[[upstreams]]
name = "a"
kind = "anthropic"

[[upstreams]]
name = "b"
kind = "anthropic"

[[routes]]
name = "split"
strategy = "weighted"

[[routes.upstreams]]
name = "a"
weight = 0.7

[[routes.upstreams]]
name = "b"
weight = 0.3
"#,
    );

    let config = load(dir.path()).unwrap();
    let route = config.routes.iter().find(|r| r.name == "split").unwrap();
    assert_eq!(route.strategy, Strategy::Weighted);
    assert_eq!(route.upstreams[0].weight, Some(0.7));
}
