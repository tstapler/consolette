//! CI gate for ADR-001's constraint that conf.d fragments stay parseable by
//! Python's `tomllib` (CD-1, NFR-2), not just the Rust `toml` crate — the two
//! parsers diverge on things like heterogeneous arrays and datetime literals.
//! Each fixture under `tests/fixtures/toml_parity/` is a real shape drawn from
//! `src/config/tests.rs` and must round-trip through both parsers.

use std::fs;
use std::process::Command;

#[test]
#[allow(clippy::unwrap_used)] // fixture directory listing / reads — a missing or unreadable
                              // fixture is a test setup bug, and the panic message from
                              // `unwrap_or_else` above already gives the real diagnostic
fn conf_d_fixtures_are_tomllib_portable() {
    let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/toml_parity");
    let mut fixtures: Vec<_> = fs::read_dir(fixtures_dir)
        .unwrap_or_else(|e| panic!("reading {fixtures_dir}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    fixtures.sort();
    assert!(
        !fixtures.is_empty(),
        "no .toml fixtures found under {fixtures_dir}"
    );

    for path in fixtures {
        let contents = fs::read_to_string(&path).unwrap();

        toml::from_str::<toml::Value>(&contents).unwrap_or_else(|e| {
            panic!("{}: rejected by the Rust `toml` crate: {e}", path.display())
        });

        let output = Command::new("python3")
            .arg("-c")
            .arg("import sys, tomllib; tomllib.load(open(sys.argv[1], 'rb'))")
            .arg(&path)
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 (required in CI): {e}"));
        assert!(
            output.status.success(),
            "{}: rejected by Python tomllib:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
