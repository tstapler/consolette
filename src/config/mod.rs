//! ADR-001: figment-based layered config engine.

mod load;
pub mod plugins;
pub mod runtime_overrides;
pub mod schema;
mod validate;

pub use load::{load, plugin_bin_dirs};
pub use runtime_overrides::RuntimeOverrides;
pub use validate::{validate_model_selectors, validate_references};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Figment(#[from] Box<figment::Error>),
    #[error("{route} references unknown upstream {upstream:?}")]
    UnknownUpstreamReference { route: String, upstream: String },
    /// A `RouteUpstreamRef` set both `model` and `model_family` (mutually
    /// exclusive selectors), or a `kind = "openai"` upstream set neither.
    #[error("route {route:?} upstream {upstream:?} sets both model and model_family (or, for a kind=\"openai\" upstream, neither) — exactly one is required")]
    ConflictingModelSelector { route: String, upstream: String },
    /// `model_family` is an internal dispatch key only `OpenaiProvider::send`
    /// knows to strip (Story 1.3.3) — setting it on any other upstream kind
    /// would leak it straight into that upstream's real request body.
    #[error("route {route:?} upstream {upstream:?} sets model_family but its kind is {kind:?}, not \"openai\"")]
    ModelFamilyOnNonOpenaiUpstream {
        route: String,
        upstream: String,
        kind: String,
    },
    #[error("failed to load runtime overrides: {0}")]
    RuntimeOverrides(String),
}

#[cfg(test)]
mod tests;
