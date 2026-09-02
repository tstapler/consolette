//! ADR-001: figment-based layered config engine.

mod load;
pub mod plugins;
pub mod runtime_overrides;
pub mod schema;
mod validate;

pub use load::{load, plugin_bin_dirs};
pub use runtime_overrides::RuntimeOverrides;
pub use validate::validate_references;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Figment(#[from] Box<figment::Error>),
    #[error("{route} references unknown upstream {upstream:?}")]
    UnknownUpstreamReference { route: String, upstream: String },
    #[error("failed to load runtime overrides: {0}")]
    RuntimeOverrides(String),
}

#[cfg(test)]
mod tests;
