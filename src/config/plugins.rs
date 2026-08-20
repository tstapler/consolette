//! ADR-007 §1: plugin discovery — `plugins.d/*/` bundles contributing
//! `conf.d/*.toml` fragments (merged after core conf.d) and `bin/`
//! credential helpers (searched ahead of `PATH` — see `crate::auth::exec`).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A plugin's `plugin.toml` manifest (ADR-007 §1). Only `name` currently
/// drives behavior (merge/lookup ordering); the rest round-trip for
/// diagnostics.
#[derive(Debug, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub provides: Vec<String>,
}

/// One discovered plugin: its manifest plus the directory it lives in.
pub struct Plugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
}

impl Plugin {
    /// This plugin's `conf.d/*.toml` fragments, sorted lexically (ADR-001's
    /// sorted-glob rule, applied within the plugin).
    fn conf_d_files(&self) -> Vec<PathBuf> {
        let pattern = self.dir.join("conf.d").join("*.toml");
        let mut files: Vec<PathBuf> = glob::glob(&pattern.to_string_lossy())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect();
        files.sort();
        files
    }

    /// This plugin's `bin/` directory, if it has one.
    fn bin_dir(&self) -> Option<PathBuf> {
        let dir = self.dir.join("bin");
        dir.is_dir().then_some(dir)
    }
}

/// Discovers plugins per ADR-007 §1: `<config_dir>/plugins.d/*/` plus any
/// directories in `CONSOLETTE_PLUGIN_PATH` (colon-separated), each requiring
/// a parseable `plugin.toml` to count as a plugin. A missing search path
/// yields no plugins — core still runs standalone. Results are sorted by
/// plugin name (lexical), matching the conf.d merge order the ADR specifies.
///
/// Invalid entries (missing/unparseable `plugin.toml`) are skipped rather
/// than erroring — a stray directory in `plugins.d/` shouldn't block startup.
pub fn discover(config_dir: &Path) -> Vec<Plugin> {
    let mut candidate_dirs: Vec<PathBuf> = Vec::new();

    let pattern = config_dir.join("plugins.d").join("*");
    candidate_dirs.extend(
        glob::glob(&pattern.to_string_lossy())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|p| p.is_dir()),
    );

    if let Ok(path_var) = std::env::var("CONSOLETTE_PLUGIN_PATH") {
        candidate_dirs.extend(std::env::split_paths(&path_var).filter(|p| p.is_dir()));
    }

    let mut plugins: Vec<Plugin> = candidate_dirs
        .into_iter()
        .filter_map(|dir| {
            let manifest_path = dir.join("plugin.toml");
            let raw = std::fs::read_to_string(manifest_path).ok()?;
            let manifest: PluginManifest = toml::from_str(&raw).ok()?;
            Some(Plugin { manifest, dir })
        })
        .collect();

    plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    plugins
}

/// Every discovered plugin's `conf.d/*.toml` fragments, in merge order
/// (plugin-name lexical, each plugin's own files sorted within it).
pub fn conf_d_files(plugins: &[Plugin]) -> Vec<PathBuf> {
    plugins.iter().flat_map(Plugin::conf_d_files).collect()
}

/// `bin/` directories of all discovered plugins, in discovery order — used to
/// extend the credential-helper search path (ADR-007 §2: "resolves against
/// the owning plugin's `bin/` first, then `PATH`"). v1 simplification: a
/// helper command is searched across *all* plugin `bin/` dirs rather than
/// tracking which plugin's `conf.d` fragment contributed a given upstream —
/// the flat config-merge design has no such provenance, and in practice a
/// helper name is unique to the plugin that ships it.
pub fn bin_dirs(plugins: &[Plugin]) -> Vec<PathBuf> {
    plugins.iter().filter_map(Plugin::bin_dir).collect()
}

#[cfg(test)]
mod tests;
