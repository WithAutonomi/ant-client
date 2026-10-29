#!/usr/bin/env bash
# Rotate CHANGELOG.md's `## [Unreleased]` entries into a `## [X.Y.Z] - <date>` section.
#
# Run this when a train cuts rc.1, on the rc branch, before tagging. The release workflow
# (`ant-cli-release.yml`) ships the section whose heading matches the tag's base version,
# so `ant-cli-vX.Y.Z-rc.N`, `-beta.N` and the final `ant-cli-vX.Y.Z` all read the
# `## [X.Y.Z]` section this script writes. See V2-1107.
#
#   scripts/rotate-changelog.sh <version> [<date>] [<changelog>]
#
#   <version>    X.Y.Z, or a prerelease X.Y.Z-rc.N (the suffix is dropped)
#   <date>       YYYY-MM-DD, defaults to today in UTC
#   <changelog>  defaults to CHANGELOG.md
#
# Exit status:
#   0  rotated; or `## [X.Y.Z]` already has entries (an rc.2 re-cut), so nothing to do;
#      or `[Unreleased]` is empty, so nothing to rotate (a warning is printed, and the
#      release workflow publishes generated notes for that version instead)
#   1  bad arguments, or the changelog has no `## [Unreleased]` heading
#   2  `## [X.Y.Z]` exists with entries AND `[Unreleased]` has entries too: someone
#      added entries after the section was rotated, and a human has to place them
#
# A changelog problem must never block a release (V2-1142): callers in the cut tooling
# should report a non-zero exit, not abort the cut on it.

set -euo pipefail

usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
}

[ $# -ge 1 ] && [ $# -le 3 ] || usage

version="${1#v}"
version="${version%%-*}"
date="${2:-$(date -u +%F)}"
changelog="${3:-CHANGELOG.md}"

if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "rotate-changelog: '$1' is not a version (want X.Y.Z or X.Y.Z-rc.N)" >&2
  exit 1
fi
if ! [[ "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
  echo "rotate-changelog: '$date' is not a date (want YYYY-MM-DD)" >&2
  exit 1
fi
if [ ! -f "$changelog" ]; then
  echo "rotate-changelog: $changelog not found" >&2
  exit 1
fi
if ! grep -q '^## \[Unreleased\]' "$changelog"; then
  echo "rotate-changelog: $changelog has no '## [Unreleased]' heading" >&2
  exit 1
fi

# Print the body of the section whose heading starts with `## [<name>]`.
section() {
  awk -v name="$1" '
    index($0, "## [" name "]") == 1 { found = 1; next }
    /^## \[/ { if (found) exit }
    found' "$changelog"
}

has_entries() {
  [ -n "$(printf '%s' "$1" | tr -d '[:space:]')" ]
}

unreleased="$(section Unreleased)"
existing="$(section "$version")"

if has_entries "$existing"; then
  if has_entries "$unreleased"; then
    echo "rotate-changelog: ## [$version] already has entries, and so does ## [Unreleased]." >&2
    echo "rotate-changelog: move the [Unreleased] entries that ship in $version by hand." >&2
    exit 2
  fi
  echo "rotate-changelog: ## [$version] already has entries; nothing to do."
  exit 0
fi

if ! has_entries "$unreleased"; then
  echo "rotate-changelog: WARNING: ## [Unreleased] is empty, so $version has no entries." >&2
  echo "rotate-changelog: the release will publish generated notes for it; add entries under" >&2
  echo "rotate-changelog: ## [$version] (and 'gh release edit' the release) to replace them." >&2
  exit 0
fi

# Keep the `[Unreleased]` heading, open `[X.Y.Z] - <date>` under it and move the entries
# there unchanged. An empty `## [X.Y.Z]` heading left by hand is dropped, not duplicated.
# The result goes to a temporary file next to the changelog, which is then renamed over it, so
# an interrupted run leaves the old changelog whole. `cp -p` gives that file the changelog's mode.
tmp="$(mktemp "$(dirname "$changelog")/.rotate-changelog.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
cp -p "$changelog" "$tmp"
awk -v ver="$version" -v date="$date" '
  index($0, "## [" ver "]") == 1 { skip_empty = 1; next }
  skip_empty && /^[[:space:]]*$/ { next }
  { skip_empty = 0 }
  !done && /^## \[Unreleased\]/ {
    print
    print ""
    print "## [" ver "] - " date
    done = 1
    next
  }
  { print }' "$changelog" > "$tmp"
mv -f "$tmp" "$changelog"

echo "rotate-changelog: moved ## [Unreleased] into ## [$version] - $date in $changelog."
