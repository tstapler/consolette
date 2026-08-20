use std::fs;
use std::sync::{Mutex, OnceLock};

use tempfile::tempdir;

use super::*;

/// One test mutates the process-global `CONSOLETTE_PLUGIN_PATH` env var that
/// `discover` reads; every test calling `discover` takes this lock so none
/// of them observe a leaked value while it's set.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn write_plugin(root: &Path, name: &str, conf_files: &[(&str, &str)], with_bin: bool) {
    let dir = root.join("plugins.d").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\ndescription = \"test\"\n"),
    )
    .unwrap();

    if !conf_files.is_empty() {
        let conf_d = dir.join("conf.d");
        fs::create_dir_all(&conf_d).unwrap();
        for (file, contents) in conf_files {
            fs::write(conf_d.join(file), contents).unwrap();
        }
    }

    if with_bin {
        fs::create_dir_all(dir.join("bin")).unwrap();
    }
}

#[test]
fn missing_plugins_d_yields_no_plugins() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    let plugins = discover(root.path());
    assert!(plugins.is_empty());
}

#[test]
fn discovers_plugin_with_valid_manifest() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    write_plugin(root.path(), "acme-vendor", &[("50-model-gateway.toml", "")], true);

    let plugins = discover(root.path());
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name, "acme-vendor");
}

#[test]
fn skips_directory_without_plugin_toml() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("plugins.d").join("not-a-plugin")).unwrap();

    let plugins = discover(root.path());
    assert!(plugins.is_empty());
}

#[test]
fn skips_directory_with_unparseable_plugin_toml() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    let dir = root.path().join("plugins.d").join("broken");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.toml"), "not valid toml {{{").unwrap();

    let plugins = discover(root.path());
    assert!(plugins.is_empty());
}

#[test]
fn plugins_are_sorted_by_name_lexically() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    write_plugin(root.path(), "zeta", &[], false);
    write_plugin(root.path(), "alpha", &[], false);

    let plugins = discover(root.path());
    let names: Vec<&str> = plugins.iter().map(|p| p.manifest.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "zeta"]);
}

#[test]
fn conf_d_files_are_collected_in_merge_order() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    write_plugin(
        root.path(),
        "acme-vendor",
        &[("50-model-gateway.toml", ""), ("10-early.toml", "")],
        false,
    );
    write_plugin(root.path(), "example", &[("60-extra.toml", "")], false);

    let plugins = discover(root.path());
    let files = conf_d_files(&plugins);

    // "acme-vendor" plugin sorts before "example", and within a plugin its own
    // files are sorted lexically regardless of write order.
    let file_names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        file_names,
        vec!["10-early.toml", "50-model-gateway.toml", "60-extra.toml"]
    );
}

#[test]
fn bin_dirs_only_includes_plugins_with_a_bin_directory() {
    let _guard = env_lock().lock().unwrap();
    let root = tempdir().unwrap();
    write_plugin(root.path(), "acme-vendor", &[], true);
    write_plugin(root.path(), "example", &[], false);

    let plugins = discover(root.path());
    let dirs = bin_dirs(&plugins);

    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0], root.path().join("plugins.d/acme-vendor/bin"));
}

#[test]
fn consolette_plugin_path_env_var_contributes_additional_plugin_dirs() {
    let _guard = env_lock().lock().unwrap();

    let root = tempdir().unwrap();
    let extra = tempdir().unwrap();
    write_plugin(extra.path(), "unused-name-not-derived-from-dir", &[], false);
    // The manifest's declared name is what's used, not the directory name.
    fs::rename(
        extra.path().join("plugins.d").join("unused-name-not-derived-from-dir"),
        extra.path().join("standalone"),
    )
    .unwrap();

    std::env::set_var(
        "CONSOLETTE_PLUGIN_PATH",
        extra.path().join("standalone"),
    );
    let plugins = discover(root.path());
    std::env::remove_var("CONSOLETTE_PLUGIN_PATH");

    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name, "unused-name-not-derived-from-dir");
}
