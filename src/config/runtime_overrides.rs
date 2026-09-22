//! Runtime-mutable route overrides layered on top of conf.d's static
//! config: a separate file, `<config_dir>/runtime-overrides.toml`, that the
//! web control panel (`POST /api/route`) writes to — kept apart from
//! hand-authored `conf.d/*.toml` so the two never fight over the same file,
//! and untouched by plugin installs.
//!
//! Applied last in [`super::load`], after the conf.d/plugins/env merge, so
//! it wins over everything else — and, being a real file, survives a
//! restart rather than resetting to whatever conf.d says (defaults live in
//! conf.d; this is the layer that overwrites them at runtime).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::schema::{Config, Route};

pub const FILE_NAME: &str = "runtime-overrides.toml";

/// A route here always models the *entire* active route, not a
/// field-by-field patch — matches conf.d's own `[[routes]]` array-replace
/// semantics (ADR-001), so applying it is just "replace `config.routes`".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOverrides {
    #[serde(default)]
    pub route: Option<Route>,
}

impl RuntimeOverrides {
    #[must_use]
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    /// Loads the runtime overrides file, if present. A missing file yields
    /// an empty (no-op) `RuntimeOverrides`, not an error — most installs
    /// never create one.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but can't be read, or fails to
    /// parse as TOML.
    pub fn load(config_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(config_dir);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                toml::from_str(&contents).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    /// Persists this override to `<config_dir>/runtime-overrides.toml`,
    /// creating `config_dir` if it doesn't already exist.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails or the file can't be written.
    pub fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        let path = Self::path(config_dir);
        let contents =
            toml::to_string_pretty(self).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
        std::fs::create_dir_all(config_dir)
            .map_err(|e| anyhow::anyhow!("{}: {e}", config_dir.display()))?;
        std::fs::write(&path, contents).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    /// Applies this override onto an already-loaded [`Config`], in place.
    pub fn apply(&self, config: &mut Config) {
        if let Some(route) = &self.route {
            config.routes = vec![route.clone()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{RouteUpstreamRef, Strategy};

    fn sample_route() -> Route {
        Route {
            name: "default".to_string(),
            strategy: Strategy::Weighted,
            upstreams: vec![RouteUpstreamRef {
                name: "bedrock".to_string(),
                weight: Some(1.0),
                model: Some("override-model".to_string()),
                model_family: None,
            }],
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn load_missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = RuntimeOverrides::load(dir.path()).unwrap();
        assert_eq!(overrides, RuntimeOverrides::default());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = RuntimeOverrides {
            route: Some(sample_route()),
        };
        overrides.save(dir.path()).unwrap();

        let loaded = RuntimeOverrides::load(dir.path()).unwrap();
        assert_eq!(loaded, overrides);
    }

    #[test]
    fn apply_replaces_routes_wholesale() {
        let overrides = RuntimeOverrides {
            route: Some(sample_route()),
        };
        let mut config = Config::default();
        assert_ne!(config.routes, vec![sample_route()]);

        overrides.apply(&mut config);
        assert_eq!(config.routes, vec![sample_route()]);
    }

    #[test]
    fn apply_with_no_route_leaves_config_untouched() {
        let overrides = RuntimeOverrides::default();
        let config_before = Config::default();
        let mut config = Config::default();

        overrides.apply(&mut config);
        assert_eq!(config, config_before);
    }
}
