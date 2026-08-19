//! Config schema: plain TOML-portable structs (no figment-only constructs),
//! so the same conf.d files load unchanged in Python `tomllib` (CD-1, NFR-2).

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

fn default_apikey_header() -> String {
    "x-api-key".to_string()
}

fn default_cache_ttl_secs() -> u64 {
    300
}

fn default_exec_timeout_secs() -> u64 {
    10
}

fn default_config_dir() -> String {
    "~/.config/consolette".to_string()
}

/// A secret value given as an indirect reference rather than inline plaintext
/// (FR-2.3). `Inline` exists for tests/local experimentation, not for
/// checked-in config.
#[derive(Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum SecretRef {
    Inline { value: String },
    Env { var: String },
    Keychain { item: String },
}

impl fmt::Debug for SecretRef {
    /// Never prints a secret value (AC12) — env/keychain reference *names*
    /// aren't secrets themselves, only the resolved value is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Inline { .. } => write!(f, "Inline(<redacted>)"),
            SecretRef::Env { var } => write!(f, "Env({var})"),
            SecretRef::Keychain { item } => write!(f, "Keychain({item})"),
        }
    }
}

impl Serialize for SecretRef {
    /// Hand-written so `Inline`'s value is redacted on serialize too — a
    /// derived impl would round-trip the plaintext secret into any future
    /// JSON/TOML dump of `Config` (e.g. a "show effective config" command),
    /// silently defeating the `Debug` redaction above (AC12).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "source", rename_all = "lowercase")]
        enum Repr<'a> {
            Inline { value: &'a str },
            Env { var: &'a str },
            Keychain { item: &'a str },
        }
        let repr = match self {
            SecretRef::Inline { .. } => Repr::Inline {
                value: "<redacted>",
            },
            SecretRef::Env { var } => Repr::Env { var },
            SecretRef::Keychain { item } => Repr::Keychain { item },
        };
        repr.serialize(serializer)
    }
}

/// Pluggable per-upstream auth (FR-2.2, ADR-007 §3). Core ships exactly three
/// types — `bearer`, `apikey`, `exec` — with no core-native mode for
/// my employer's internal identity system: that auth is delivered as a plugin's
/// `exec` credential-helper instead of a core-native auth mode (plan.md Task 4 / AC3).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum AuthMethod {
    Bearer {
        token: SecretRef,
    },
    Apikey {
        key: SecretRef,
        #[serde(default = "default_apikey_header")]
        header: String,
    },
    /// Generic credential-helper (ADR-007 §2). `command` resolves against the
    /// owning plugin's `bin/` first, then `PATH`. Dispatch (subprocess
    /// invocation, caching, header injection) lands with the ADR-007 plugin
    /// runtime — see `auth::AuthMethodExt::apply`.
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_cache_ttl_secs")]
        cache_ttl_secs: u64,
        #[serde(default = "default_exec_timeout_secs")]
        timeout_secs: u64,
    },
}

/// Kind-specific upstream configuration (FR-2.1). Bedrock options are a typed
/// struct, not a raw `toml::Value` bag, so `deny_unknown_fields` actually
/// catches typos there too (closes architecture-review N10).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum UpstreamKind {
    Anthropic,
    Bedrock {
        #[serde(default)]
        aws_region: Option<String>,
        #[serde(default)]
        aws_profile: Option<String>,
        #[serde(default)]
        max_retries: Option<u32>,
    },
    Openai {
        base_url: String,
    },
}

// Note: no `deny_unknown_fields` here — serde does not support combining it
// with `flatten` on the same struct. Typo protection for kind-specific
// fields still comes from `UpstreamKind`'s own `deny_unknown_fields`, since
// any field this struct doesn't recognize is handed to the flattened enum.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Upstream {
    pub name: String,
    #[serde(flatten)]
    pub kind: UpstreamKind,
    /// `None` for upstreams (e.g. Bedrock) authenticated by an ambient
    /// credential chain rather than a bearer/apikey secret.
    #[serde(default)]
    pub auth: Option<AuthMethod>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Fallback,
    Weighted,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RouteUpstreamRef {
    pub name: String,
    #[serde(default)]
    pub weight: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub strategy: Strategy,
    pub upstreams: Vec<RouteUpstreamRef>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnBreach {
    #[default]
    Shed,
    Delay,
}

fn default_max_delay_ms() -> u64 {
    2000
}

/// Per-upstream rate-limit dimensions (ADR-004). `on_breach`/`max_delay_ms`
/// are `Option` here so an unset field falls back to
/// `RateLimitConfig.defaults`, then to the built-in default
/// (`shed`/2000ms) — see `RateLimitConfig::resolved_breach`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
    #[serde(default)]
    pub on_breach: Option<OnBreach>,
    #[serde(default)]
    pub max_delay_ms: Option<u64>,
}

/// `[ratelimit.defaults]` — `on_breach`/`max_delay_ms` shared across
/// upstreams that don't override them (ADR-004).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RateLimitDefaults {
    #[serde(default)]
    pub on_breach: OnBreach,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RateLimitDefaults {
    fn default() -> Self {
        Self {
            on_breach: OnBreach::default(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

/// `[ratelimit]` as a whole — table-of-tables (`defaults` + `upstreams.<name>`),
/// not an array-of-tables, so conf.d layering merges rather than replaces
/// (ADR-001, ADR-004).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub defaults: RateLimitDefaults,
    #[serde(default)]
    pub upstreams: HashMap<String, RateLimit>,
}

impl RateLimitConfig {
    /// Resolves the effective `on_breach`/`max_delay_ms` for `upstream`:
    /// per-upstream override, else `defaults`.
    #[must_use]
    pub fn resolved_breach(&self, upstream: &str) -> (OnBreach, u64) {
        let limit = self.upstreams.get(upstream);
        let on_breach = limit
            .and_then(|l| l.on_breach)
            .unwrap_or(self.defaults.on_breach);
        let max_delay_ms = limit
            .and_then(|l| l.max_delay_ms)
            .unwrap_or(self.defaults.max_delay_ms);
        (on_breach, max_delay_ms)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
// `config_dir` naturally shares a prefix with the struct name; renaming it
// would be a breaking config-file change for no clarity gain.
#[allow(clippy::struct_field_names)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_log")]
    pub log: String,
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: u64,
    #[serde(default = "default_config_dir")]
    pub config_dir: String,
    #[serde(default = "default_true")]
    pub compress: bool,
    #[serde(default = "default_compress_floor_bytes")]
    pub compress_floor_bytes: u64,
    #[serde(default)]
    pub cache_aligner: bool,
    #[serde(default = "default_verbosity_level")]
    pub verbosity_level: u8,
    #[serde(default = "default_memory_max_entries")]
    pub memory_max_entries: usize,
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub ratelimit: RateLimitConfig,
    #[serde(default)]
    pub cost_metrics: CostMetricsConfig,
}

/// `serve-cost`'s config-file surface (Epic 2.3, Story 2.3.1): the
/// dedicated `--port` flag falls back to `[cost_metrics].port` here, then to
/// `crate::cost_metrics::server::DEFAULT_PORT`, so there's one config-aware
/// port path rather than a second, config-blind flag-only one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct CostMetricsConfig {
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_port() -> u16 {
    47000
}
fn default_log() -> String {
    "info".to_string()
}
fn default_request_timeout() -> u64 {
    60
}
fn default_cooldown_seconds() -> u64 {
    300
}
fn default_true() -> bool {
    true
}
fn default_compress_floor_bytes() -> u64 {
    1024
}
fn default_verbosity_level() -> u8 {
    1
}
fn default_memory_max_entries() -> usize {
    1000
}

impl Default for Config {
    /// An empty conf.d must yield a working config (FR-1.3) that reproduces
    /// today's Anthropic-primary/Bedrock-fallback behavior (FR-2.4): the
    /// implicit `anthropic`/`bedrock` upstreams and a `default` fallback
    /// route exist even with zero files on disk.
    fn default() -> Self {
        Config {
            port: default_port(),
            log: default_log(),
            request_timeout: default_request_timeout(),
            cooldown_seconds: default_cooldown_seconds(),
            config_dir: default_config_dir(),
            compress: true,
            compress_floor_bytes: default_compress_floor_bytes(),
            cache_aligner: false,
            verbosity_level: default_verbosity_level(),
            memory_max_entries: default_memory_max_entries(),
            upstreams: vec![
                Upstream {
                    name: "anthropic".to_string(),
                    kind: UpstreamKind::Anthropic,
                    auth: Some(AuthMethod::Bearer {
                        token: SecretRef::Env {
                            var: "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
                        },
                    }),
                },
                Upstream {
                    name: "bedrock".to_string(),
                    kind: UpstreamKind::Bedrock {
                        aws_region: None,
                        aws_profile: None,
                        max_retries: None,
                    },
                    auth: None,
                },
            ],
            routes: vec![Route {
                name: "default".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![
                    RouteUpstreamRef {
                        name: "anthropic".to_string(),
                        weight: None,
                    },
                    RouteUpstreamRef {
                        name: "bedrock".to_string(),
                        weight: None,
                    },
                ],
            }],
            ratelimit: RateLimitConfig::default(),
            cost_metrics: CostMetricsConfig::default(),
        }
    }
}
