//! ADR-001: figment-based layered config engine.

mod load;
pub mod plugins;
pub mod schema;
mod validate;

pub use load::{load, plugin_bin_dirs};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Figment(#[from] Box<figment::Error>),
    #[error("{route} references unknown upstream {upstream:?}")]
    UnknownUpstreamReference { route: String, upstream: String },
}

#[cfg(test)]
mod tests;
