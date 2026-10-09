# Build-system plan: cargo-native UI rebuild first, Bazel as a gated second step

## Problem

`ui/dist` is gitignored. `build.rs` embeds a "Loading..." stub when it is missing, and
`rust-embed` (`src/dashboard.rs`, `#[folder = "ui/dist/consolette/browser"]`) bakes in whatever
is there. Two incidents on 2026-10-09 came from the Rust build not knowing the UI is an input:
a deployed binary with the stub dashboard, then a real bundle built with the wrong `<base href>`.
Today `make ui` (stamp files) fixes the first for `make install` only — plain `cargo build` /
`cargo install` still embed whatever is on disk.

## Decision gates

| Gate | Question | If yes | If no |
|---|---|---|---|
| G0 | Does a cargo-native `build.rs` UI step make `cargo build/install/test` rebuild the UI correctly and only when inputs change? | Stop. Bazel is optional. | Go to Phase 1. |
| G1 | Do we want a hermetic, cached, single-graph build (UI + Rust + tests) badly enough to give up `cargo-dist` installers? | Phase 1–3 | Stay on Phase 0. |

## Phase 0 — cargo-native (≈ half a day)

`build.rs` is already the hook cargo gives us. Extend it instead of a Makefile:

1. Emit `cargo:rerun-if-changed` for `ui/src`, `ui/public`, `ui/package-lock.json`,
   `ui/package.json`, `ui/angular.json`, `ui/tsconfig*.json` (today it watches only `ui/src` and
   the dist dir).
2. When those are newer than `ui/dist/.stamp`, run `npm ci` (if `node_modules` is stale) and
   `npm run build` from `build.rs`; write the stamp. If `node` is missing, keep the stub **but
   `cargo:warning=` loudly** (and fail under `CONSOLETTE_REQUIRE_UI=1`, set in CI/release).
3. Keep `make ui` as a thin alias or delete it; `make install` then needs no UI knowledge.
4. Keep `embedded_index_assets_resolve_under_dashboard_base` (`src/dashboard.rs`) as the guard
   against the base-href class of bug.

Limits (INFERRED, not yet tested): `build.rs` that shells to npm makes `cargo install --git`
require Node, and it runs on every dependent crate's build. Acceptable for a private
single-crate app; the release path (`cargo-dist`) already installs Node.

Exit test for G0: touch one file in `ui/src`, run `cargo build`, confirm exactly the UI
rebuilds and the binary re-embeds; touch nothing, confirm a no-op.

## Phase 1 — Bazel spike (≈ 2–3 days, throwaway branch)

Goal: prove the two risky pieces before committing.

- `MODULE.bazel` with `rules_rust` + `crate_universe` fed from `Cargo.lock`
  (`crate.from_cargo`), pinned toolchain matching `rust-toolchain.toml`.
- `rules_js` + `npm_translate_lock` on `ui/package-lock.json`, a `js_run_binary` (or
  `aspect_rules_ts`) target running `ng build --base-href /dashboard/` and producing a tree
  artifact.
- **Risk 1 — `rust-embed`.** It reads `ui/dist/...` from the source tree at compile time.
  Under Bazel the UI is a build output, so pass it as `compile_data` and point the macro at the
  sandbox path, or swap to a generated Rust module. Prove this first; it decides feasibility.
- **Risk 2 — native deps.** `rusqlite` with `bundled` needs a C toolchain; `aws-sdk-*` makes
  the crate graph large. `crate_universe` handles `build.rs` via `cargo_build_script`, but
  confirm both link on macOS arm64 and Linux.

Spike exit: `bazel build //:consolette` produces a binary whose `/dashboard` serves the real
SPA, and `bazel test //...` runs the 1044-test lib suite.

## Phase 2 — Real migration (≈ 1 week)

- Targets: `consolette` (lib + 4 bins), tests (`rust_test` per crate; integration tests in
  `tests/`), clippy and rustfmt via `rules_rust` aspects.
- Dev loop: `bazel run //:consolette`; rust-analyzer via `gen_rust_project`. Keep `Cargo.toml`
  as the dependency source of truth (crate_universe reads it) so `cargo` keeps working for
  editors and quick checks.
- `make install` becomes `bazel build //:consolette` + copy + `codesign` (the existing
  re-sign workaround still applies).
- CI (`.github/workflows/ci.yml`): `bazel test //...` with a remote or disk cache.

## Phase 3 — Release (the expensive part)

`cargo-dist` drives `release.yml` (generated; four targets: aarch64/x86_64 darwin,
x86_64/aarch64 linux-gnu; Homebrew tap publish with a hand-edited GitHub-App-token step —
`dist-workspace.toml`). It cannot drive Bazel. Options:

- **A (recommended if we proceed):** keep `cargo-dist` for release, using `build.rs` from
  Phase 0. Bazel owns dev/CI only. Two build systems, but release stays supported tooling.
- **B:** replace `cargo-dist` with Bazel + `rules_pkg` + hand-written release workflow, cross
  toolchains for linux-aarch64, and our own Homebrew formula publishing. Highest cost; only if
  hermetic release builds are a hard requirement.

## Recommendation

Do Phase 0 now: it removes the actual failure class (UI not tracked as a Rust build input)
for far less than a Bazel migration, and keeps `cargo-dist`. Run the Phase 1 spike only if the
team wants hermetic caching across more languages or services; Risk 1 (`rust-embed`) and the
Phase 3 release split are the two reasons it could be a bad trade for a single-crate app.

## Unverified

Everything under "Limits" and Risks 1–2 is reasoning from the repo's shape, not tested. Bazel
module versions and the exact `rules_js`/`rules_rust` attribute names should be taken from
their current docs when the spike starts. Local tooling present: `bazel`/`bazelisk` at
`/opt/homebrew/bin`.
