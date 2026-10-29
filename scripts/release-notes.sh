#!/usr/bin/env bash
# Write the GitHub release notes for an `ant-cli-v*` tag. The release workflow
# (`ant-cli-release.yml`) calls this; `scripts/test-release-notes.sh` tests it. See V2-1107.
#
#   scripts/release-notes.sh <version> <tag> <out-file> [<changelog>]
#
#   <version>    the tag's version: X.Y.Z, or a prerelease X.Y.Z-rc.N / X.Y.Z-beta.N
#   <tag>        the tag being released, e.g. ant-cli-v0.3.12-rc.1
#   <out-file>   where to write the notes
#   <changelog>  defaults to CHANGELOG.md
#
# The notes are, in order of preference:
#   1. The changelog section for the tag's base version, `## [X.Y.Z]`. NOT "the first section":
#      blindly taking the first section shipped the ever-growing `[Unreleased]` blob as the notes
#      of every release from 0.2.x through 0.3.4 (0.3.4's notes were byte-identical to 0.3.3's;
#      the dual-stack entry re-shipped for four months). `-rc.N` / `-beta.N` builds share the
#      base version's section, which the train creates when it cuts rc.1
#      (`scripts/rotate-changelog.sh X.Y.Z`), so every prerelease and the final stable of one
#      train ship the same, correct, notes.
#   2. Without a section, GitHub's generated list of the PRs merged since the previous stable
#      `ant-cli-v*` release, so a missed rotation ships the right list of changes rather than
#      none (0.3.7 to 0.3.12 all shipped the placeholder).
#   3. A placeholder, when there is no previous stable release or the API call fails.
#
# A missing or empty section must never stop a release. The artifacts are fine and the notes are
# fixable after the fact with `gh release edit`, so failing closed here would only turn "missing
# notes" into "no release at all". The `::warning::` annotations keep the omission loud on the
# run. Pre-tag enforcement, if we want it, belongs on the release-prep PR where a fix is cheap and
# nothing is blocked. So this exits 0 whatever the API call does, and 1 only on bad arguments or
# a missing changelog.
#
# The API call needs GITHUB_REPOSITORY and GH_TOKEN, and the tags are listed from `origin`.

set -euo pipefail

if [ $# -lt 3 ] || [ $# -gt 4 ]; then
  sed -n '5,10p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
fi

version="$1"
tag="$2"
out="$3"
changelog="${4:-CHANGELOG.md}"
base_version="${version%%-*}"

if [ ! -f "$changelog" ]; then
  echo "release-notes: $changelog not found" >&2
  exit 1
fi

section=$(awk -v ver="$base_version" '
  BEGIN { gsub(/\./, "\\.", ver); pat = "^## \\[" ver "\\]" }
  $0 ~ pat { found = 1; next }
  /^## \[/ { if (found) exit }
  found' "$changelog")

if [ -n "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  printf '%s\n' "$section" > "$out"
  exit 0
fi

# Highest stable tag below this base version. For `X.Y.Z-rc.N` and for `X.Y.Z` alike that is the
# previous train's stable, so every build of one train lists the same changes.
previous_tag=$(git ls-remote --tags --refs origin 'ant-cli-v*' \
  | sed 's|.*refs/tags/||' \
  | grep -E '^ant-cli-v[0-9]+\.[0-9]+\.[0-9]+$' \
  | { cat; echo "ant-cli-v${base_version}"; } \
  | sort -u -V \
  | grep -B1 -x "ant-cli-v${base_version}" \
  | grep -v -x "ant-cli-v${base_version}" || true)

generated=""
if [ -n "$previous_tag" ] && [ -n "${GITHUB_REPOSITORY:-}" ]; then
  # Only a successful call counts. On an HTTP error `gh api` skips `--jq`, prints the raw JSON
  # error body and exits non-zero, and that body would otherwise ship as the notes.
  if ! generated=$(gh api "repos/${GITHUB_REPOSITORY}/releases/generate-notes" \
    -f tag_name="$tag" -f previous_tag_name="$previous_tag" --jq .body); then
    generated=""
  fi
fi

if [ -n "$(printf '%s' "$generated" | tr -d '[:space:]')" ]; then
  echo "::warning title=Missing changelog section::CHANGELOG.md has no non-empty '## [${base_version}]' section. Publishing GitHub's generated notes (${previous_tag}..${tag}); replace them with 'gh release edit' once the section exists, and rotate '## [Unreleased]' with 'scripts/rotate-changelog.sh ${base_version}' at the cut. See V2-1107."
  {
    echo "_No changelog entry was recorded for ${base_version}. These are the pull requests merged since ${previous_tag#ant-cli-v}; the notes will be corrected retroactively._"
    echo
    printf '%s\n' "$generated"
  } > "$out"
else
  echo "::warning title=Missing changelog section::CHANGELOG.md has no non-empty '## [${base_version}]' section, and GitHub's generated notes were unavailable. Publishing with placeholder notes; correct them with 'gh release edit' and rotate '## [Unreleased]' with 'scripts/rotate-changelog.sh ${base_version}' at the cut. See V2-1107."
  echo "_No changelog entry was recorded for ${base_version}. See the commit history for the changes in this release; these notes will be corrected retroactively._" > "$out"
fi
