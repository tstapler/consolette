# Releasing consolette

Follows the kibitzer/stelekit/stapler-squad pattern: `cargo-dist` builds and
publishes, `git-cliff` writes the changelog, and version tags are pushed via
an interactively-authenticated push (a human, or Claude Code operating with
the user's own git/gh credentials in an interactive session) — never from
an automated CI bot.

## Why the tag can't come from CI

release-please (and similar bots) commit and push using `GITHUB_TOKEN`, and
GitHub deliberately does not let a `GITHUB_TOKEN`-authored push trigger other
`on: push` workflows (it's a loop-prevention measure, not a bug). That means
a bot-pushed tag would never kick off `release.yml`. The constraint is about
*how* the push is authenticated, not *who* runs the command: Claude Code may
cut a release the same way a human would — `git tag` + `git push` using the
session's own authenticated git remote — since that's an interactive push,
not a `GITHUB_TOKEN`-authored one. It still shouldn't push a tag on its own
initiative; only when the user asks for a release. The version number is not
a per-release question: it is whatever `scripts/next-version.sh` prints.

## One-time setup

1. `cargo install cargo-dist` then `dist init` — answer its prompts, it will
   read `dist-workspace.toml` and may rewrite it against the installed
   version.
2. `dist generate` — writes `.github/workflows/release.yml`. Do not
   hand-write that workflow; regenerate it whenever `dist-workspace.toml`
   changes.
3. If publishing to a Homebrew tap: create `tstapler/homebrew-tap` on
   GitHub if it doesn't exist yet, and add a `HOMEBREW_TAP_TOKEN` repo secret
   (a PAT with push access to that tap repo).
4. If publishing to crates.io: `cargo login`, and add a `CARGO_REGISTRY_TOKEN`
   repo secret for CI.
5. If publishing to npm (e.g. wrapping the binary for `npx`): add an
   `NPM_TOKEN` repo secret.

## Cutting a release

The version comes from the Conventional Commits since the last tag, via
`scripts/next-version.sh` (wraps `git cliff --bumped-version`; rules in the `[bump]`
table of `cliff.toml`: `feat` bumps minor, `fix`/`perf`/`refactor` bump patch, and a breaking
change bumps minor until 1.0). It exits 2 with "no releasable commits" when there is
nothing to ship; `docs`/`test`/`chore`/`style` commits alone don't release.

```bash
VERSION=$(scripts/next-version.sh) || exit    # e.g. 0.4.0
sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml   # first match = [package]; GNU sed: drop the ''
cargo update --workspace --offline            # refresh Cargo.lock's package version
git cliff --tag "v$VERSION" -o CHANGELOG.md   # update the changelog
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore: release v$VERSION"
git tag "v$VERSION"
git push origin main "v$VERSION"
```

cargo-dist requires the tag to match the `Cargo.toml` version, which is why the release commit
bumps both (see the `chore: release v0.3.0` commit for the shape).

The tag push triggers `release.yml`: cross-compiles for the configured
targets, creates the GitHub Release (using `CHANGELOG.md` for notes), and
runs whichever `publish-jobs` are configured in `dist-workspace.toml`.
