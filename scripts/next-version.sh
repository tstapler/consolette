#!/usr/bin/env bash
# Prints the next release version (no leading "v") derived from the Conventional Commits since
# the latest vX.Y.Z tag, using the [bump] rules in cliff.toml. Exits 2 when nothing is releasable.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

last_tag=$(git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true)
next=$(git cliff --unreleased --bumped-version 2>/dev/null || true)
next=${next#v}

if [[ -z "$next" || "$next" == "${last_tag#v}" ]]; then
  echo "no releasable commits since ${last_tag:-the beginning}" >&2
  exit 2
fi
echo "$next"
