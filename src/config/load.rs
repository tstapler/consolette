//! Layered loader: `defaults < conf.d/*.toml (sorted) < plugins.d/*/conf.d
//! (plugin-name lexical, ADR-007 §1) < env allowlist` (ADR-001), followed by
//! the legacy env back-compat shim and reference validation.

use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;

use super::plugins;
use super::runtime_overrides::RuntimeOverrides;
use super::schema::{Config, Upstream, UpstreamKind};
use super::validate::{validate_model_selectors, validate_references};
use super::ConfigError;

/// Env overrides are restricted to a small allowlist of top-level scalars —
/// arrays-of-tables (`upstreams`/`routes`) are file-only (ADR-001).
const ENV_ALLOWLIST: &[&str] = &["port", "log", "request_timeout", "cooldown_seconds"];

/// Load config from `<config_dir>/conf.d/*.toml`, sorted lexically, deep-merged
/// over built-in defaults, with `CONSOLETTE_`-prefixed env vars as the
/// highest-precedence overlay (FR-1.1, FR-1.2).
///
/// # Errors
///
/// Returns [`ConfigError`] if a conf.d file can't be read/parsed, if
/// reference validation (upstream/route cross-references) fails, or if a
/// route upstream's `model`/`model_family` selectors are invalid (Story
/// 1.2.2).
pub fn load(config_dir: &Path) -> Result<Config, ConfigError> {
    let conf_d = config_dir.join("conf.d");
    let pattern = conf_d.join("*.toml");
    // `glob` returns filesystem order, not sorted — sort explicitly (ADR-001).
    // A missing conf.d directory simply yields zero files, not an error.
    let mut files: Vec<PathBuf> = glob::glob(&pattern.to_string_lossy())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect();
    files.sort();

    let discovered_plugins = plugins::discover(config_dir);
    let plugin_files = plugins::conf_d_files(&discovered_plugins);

    let mut figment = Figment::new().merge(Serialized::defaults(Config::default()));
    for file in files.iter().chain(plugin_files.iter()) {
        figment = figment.merge(Toml::file(file));
    }
    figment = figment.merge(Env::prefixed("CONSOLETTE_").split("__").only(ENV_ALLOWLIST));

    let mut config: Config = figment.extract().map_err(Box::new)?;
    apply_legacy_env_shim(&mut config);

    let overrides = RuntimeOverrides::load(config_dir)
        .map_err(|e| ConfigError::RuntimeOverrides(e.to_string()))?;
    overrides.apply(&mut config);

    validate_references(&config)?;
    validate_model_selectors(&config)?;
    Ok(config)
}

/// `bin/` directories of every plugin discovered under `<config_dir>/plugins.d/`
/// (plus `CONSOLETTE_PLUGIN_PATH`), for credential-helper command resolution
/// ahead of `PATH` (ADR-007 §2). Separate from [`load`] since callers that
/// only need config don't need to re-walk `plugins.d/`.
#[must_use]
pub fn plugin_bin_dirs(config_dir: &Path) -> Vec<PathBuf> {
    plugins::bin_dirs(&plugins::discover(config_dir))
}

/// Reads one legacy (unprefixed) env var, emitting a one-time deprecation
/// warning when present. `None` when unset.
fn legacy_env(name: &'static str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => {
            tracing::warn!(
                var = name,
                "legacy env var is deprecated; migrate to conf.d TOML config"
            );
            Some(value)
        }
        Err(_) => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn bedrock_upstream_mut(config: &mut Config) -> Option<&mut Upstream> {
    config
        .upstreams
        .iter_mut()
        .find(|u| matches!(u.kind, UpstreamKind::Bedrock { .. }))
}

/// Maps every legacy env var the old `Config::from_env()` read onto its new
/// home (ADR-001 Consequences), so the existing plist keeps working through
/// the Epic 6 cutover. AWS region/profile only apply when conf.d left the
/// field unset — conf.d always wins over the back-compat shim.
fn apply_legacy_env_shim(config: &mut Config) {
    if let Some(v) = legacy_env("PROXY_PORT") {
        if let Ok(parsed) = v.parse() {
            config.port = parsed;
        }
    }
    if let Some(v) = legacy_env("COOLDOWN_SECONDS") {
        if let Ok(parsed) = v.parse() {
            config.cooldown_seconds = parsed;
        }
    }
    if let Some(v) = legacy_env("REQUEST_TIMEOUT") {
        if let Ok(parsed) = v.parse() {
            config.request_timeout = parsed;
        }
    }
    if let Some(v) = legacy_env("STAPLER_COMPRESS") {
        if let Some(parsed) = parse_bool(&v) {
            config.compress = parsed;
        }
    }
    if let Some(v) = legacy_env("COMPRESS_FLOOR_BYTES") {
        if let Ok(parsed) = v.parse() {
            config.compress_floor_bytes = parsed;
        }
    }
    if let Some(v) = legacy_env("CACHE_ALIGNER") {
        if let Some(parsed) = parse_bool(&v) {
            config.cache_aligner = parsed;
        }
    }
    if let Some(v) = legacy_env("VERBOSITY_LEVEL") {
        if let Ok(parsed) = v.parse() {
            config.verbosity_level = parsed;
        }
    }
    if let Some(v) = legacy_env("MEMORY_MAX_ENTRIES") {
        if let Ok(parsed) = v.parse() {
            config.memory_max_entries = parsed;
        }
    }
    if let Some(v) = legacy_env("BEDROCK_MAX_RETRIES") {
        if let Ok(parsed) = v.parse::<u32>() {
            if let Some(upstream) = bedrock_upstream_mut(config) {
                if let UpstreamKind::Bedrock { max_retries, .. } = &mut upstream.kind {
                    *max_retries = Some(parsed);
                }
            }
        }
    }
    if let Some(v) = legacy_env("AWS_REGION") {
        if let Some(upstream) = bedrock_upstream_mut(config) {
            if let UpstreamKind::Bedrock { aws_region, .. } = &mut upstream.kind {
                if aws_region.is_none() {
                    *aws_region = Some(v);
                }
            }
        }
    }
    if let Some(v) = legacy_env("AWS_PROFILE") {
        if let Some(upstream) = bedrock_upstream_mut(config) {
            if let UpstreamKind::Bedrock { aws_profile, .. } = &mut upstream.kind {
                if aws_profile.is_none() {
                    *aws_profile = Some(v);
                }
            }
        }
    }
    // CLAUDE_CODE_OAUTH_TOKEN needs no config mutation: the default
    // `anthropic` upstream's auth already resolves via
    // `SecretRef::Env { var: "CLAUDE_CODE_OAUTH_TOKEN" }` by name. Read it
    // anyway so its presence still surfaces the one-time deprecation warning.
    let _ = legacy_env("CLAUDE_CODE_OAUTH_TOKEN");
}
