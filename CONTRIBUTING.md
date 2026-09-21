# Contributing

## Development setup

```bash
git clone https://github.com/tstapler/consolette.git
cd consolette
lefthook install   # activate pre-commit/pre-push hooks
```

## Running tests

```bash
cargo test --workspace
```

## Code style

Formatting and lints are enforced in CI (`.github/workflows/ci.yml`) and by the `lefthook` pre-push hook:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Run `cargo fmt --all` to fix formatting, and address clippy findings before opening a PR — CI denies all clippy warnings.

## Keeping the README accurate

`cargo run --bin readme-check` verifies the "## CLI reference" table matches the real `consolette --help` output and that the config example under "## Configuration" actually parses. It runs in CI on every PR — if you add, rename, or remove a CLI subcommand, or change the config schema in a way that breaks the example, update `README.md` until this passes.

## Submitting a change

1. Fork the repo and create a branch off `main`.
2. Make your change, with tests covering new behavior.
3. Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` locally.
4. Open a PR against `main`. CI runs the same three checks.

For a change to release tooling or versioning, see [RELEASE.md](RELEASE.md) first — `dist-workspace.toml` and the generated `.github/workflows/release.yml` should not be hand-edited independently of each other.

## Reporting a bug

Open a GitHub issue using the bug report template. Include the `consolette` version (`consolette --version`), your OS, and steps to reproduce.
