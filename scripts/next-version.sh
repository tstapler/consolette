#!/usr/bin/env bash
# Prints the next release version (no leading "v") derived from the Conventional Commits since
# the latest vX.Y.Z tag, using the [bump] rules in cliff.toml. Exits 2 when nothing is releasable,
# 1 on any real failure (missing git-cliff, bad config) so a gate can't mistake it for "nothing".
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

command -v git-cliff >/dev/null || { echo "git-cliff is not installed" >&2; exit 1; }

last_tag=$(git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true)
next=$(git cliff --unreleased --bumped-version 2>/dev/null) || {
  # git-cliff exits non-zero when there are no unreleased commits; re-run only to tell that apart
  # from a real failure.
  if [[ -z "$(git log --oneline "${last_tag:+$last_tag..}HEAD" | head -1)" ]]; then
    echo "no commits since ${last_tag:-the beginning}" >&2
    exit 2
  fi
  git cliff --unreleased --bumped-version >/dev/null
  exit 1
}
next=${next#v}

if [[ -z "$next" || "$next" == "${last_tag#v}" ]]; then
  echo "no releasable commits since ${last_tag:-the beginning}" >&2
  exit 2
fi
echo "$next"
