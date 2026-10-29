#!/usr/bin/env bash
# Tests for scripts/release-notes.sh, with `git` and `gh` replaced by fakes on PATH. CI runs this
# in the `Release scripts` job.
#
#   scripts/test-release-notes.sh [<release-notes.sh>]

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="${1:-$here/release-notes.sh}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"

# `git ls-remote` lists the tags in $FAKE_TAGS.
cat > "$work/bin/git" <<'EOF'
#!/usr/bin/env bash
[ "$1" = ls-remote ] || { echo "fake git: unexpected: $*" >&2; exit 2; }
for t in $FAKE_TAGS; do printf '%040d\trefs/tags/%s\n' 0 "$t"; done
EOF
# `gh` logs its arguments, prints $FAKE_GH_OUT and exits with $FAKE_GH_EXIT.
cat > "$work/bin/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_GH_LOG"
printf '%s' "$FAKE_GH_OUT"
exit "$FAKE_GH_EXIT"
EOF
chmod +x "$work/bin/git" "$work/bin/gh"
export PATH="$work/bin:$PATH" GITHUB_REPOSITORY=WithAutonomi/ant-client FAKE_GH_LOG="$work/gh.log"
export FAKE_TAGS="ant-cli-v0.3.10 ant-cli-v0.3.11-rc.1 ant-cli-v0.3.11 ant-cli-v0.3.12-rc.1"

cat > "$work/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

### Added
- Something not released yet (#300)

## [0.3.13] - 2026-11-05

## [0.3.11] - 2026-10-16

### Fixed
- A fix that shipped in 0.3.11 (#210)

## [0.3.10] - 2026-10-09

### Added
- A feature that shipped in 0.3.10 (#200)
EOF

pass=0 fail=0
ok() { echo "ok    $*"; pass=$((pass + 1)); }
bad() { echo "FAIL  $*"; fail=$((fail + 1)); }

# run <version> <tag>: runs the script with the current fakes; sets $rc, $log and $notes.
run() {
  : > "$FAKE_GH_LOG"
  rm -f "$work/notes.md"
  rc=0
  log="$(bash "$script" "$1" "$2" "$work/notes.md" "$work/CHANGELOG.md" 2>&1)" || rc=$?
  notes="$(cat "$work/notes.md" 2> /dev/null || true)"
}
gh_called() { [ -s "$FAKE_GH_LOG" ]; }
placeholder() { [ "$notes" = "_No changelog entry was recorded for $1. See the commit history for the changes in this release; these notes will be corrected retroactively._" ]; }

generated_body=$'## What\'s Changed\n* fix(core): something by @someone in #229'
error_body='{"message":"Not Found","documentation_url":"https://docs.github.com/rest/releases/releases#generate-release-notes-content-for-a-release","status":"404"}'

# 1. A section for the version is used as it is, from the line after its heading, without
#    calling the API.
FAKE_GH_OUT="$generated_body" FAKE_GH_EXIT=0 run 0.3.11 ant-cli-v0.3.11
if [ "$rc" = 0 ] && [ "$notes" = $'\n### Fixed\n- A fix that shipped in 0.3.11 (#210)' ] && ! gh_called; then
  ok "a stable tag ships its own section"
else bad "a stable tag ships its own section (rc=$rc, notes=$notes)"; fi

# 2. A prerelease reads its base version's section.
FAKE_GH_OUT="$generated_body" FAKE_GH_EXIT=0 run 0.3.11-rc.1 ant-cli-v0.3.11-rc.1
if [ "$rc" = 0 ] && printf '%s' "$notes" | grep -q '(#210)' && ! gh_called; then
  ok "an rc tag ships its base version's section"
else bad "an rc tag ships its base version's section (rc=$rc, notes=$notes)"; fi

# 3. 0.3.1 must not match the `## [0.3.11]` heading.
FAKE_GH_OUT="" FAKE_GH_EXIT=1 run 0.3.1 ant-cli-v0.3.1
if [ "$rc" = 0 ] && ! printf '%s' "$notes" | grep -q '(#210)'; then
  ok "0.3.1 does not take 0.3.11's section"
else bad "0.3.1 does not take 0.3.11's section (notes=$notes)"; fi

# 4. No section: GitHub's generated notes since the previous stable tag.
FAKE_GH_OUT="$generated_body" FAKE_GH_EXIT=0 run 0.3.12-rc.1 ant-cli-v0.3.12-rc.1
if [ "$rc" = 0 ] \
  && grep -q 'tag_name=ant-cli-v0.3.12-rc.1' "$FAKE_GH_LOG" \
  && grep -q 'previous_tag_name=ant-cli-v0.3.11' "$FAKE_GH_LOG" \
  && [ "$(printf '%s\n' "$notes" | head -1)" = "_No changelog entry was recorded for 0.3.12. These are the pull requests merged since 0.3.11; the notes will be corrected retroactively._" ] \
  && printf '%s' "$notes" | grep -qxF '* fix(core): something by @someone in #229' \
  && printf '%s' "$log" | grep -q "Publishing GitHub's generated notes"; then
  ok "a missing section gets the generated notes since the previous stable"
else bad "a missing section gets the generated notes since the previous stable (rc=$rc, gh: $(cat "$FAKE_GH_LOG"), notes=$notes)"; fi

# 5. An empty hand-written heading counts as missing.
FAKE_GH_OUT="$generated_body" FAKE_GH_EXIT=0 run 0.3.13 ant-cli-v0.3.13
if [ "$rc" = 0 ] && gh_called && printf '%s' "$notes" | grep -q '#229'; then
  ok "an empty section heading falls back to the generated notes"
else bad "an empty section heading falls back to the generated notes (notes=$notes)"; fi

# 6. The API call fails with a JSON error body: placeholder, and the error is not published.
FAKE_GH_OUT="$error_body" FAKE_GH_EXIT=1 run 0.3.12-rc.1 ant-cli-v0.3.12-rc.1
if [ "$rc" = 0 ] && gh_called && placeholder 0.3.12 \
  && ! printf '%s' "$notes" | grep -qE 'Not Found|documentation_url' \
  && printf '%s' "$log" | grep -q 'Publishing with placeholder notes'; then
  ok "a failed API call gets the placeholder, not its error body"
else bad "a failed API call gets the placeholder, not its error body (rc=$rc, notes=$notes)"; fi

# 7. A successful call with an empty body: placeholder.
FAKE_GH_OUT=$'\n' FAKE_GH_EXIT=0 run 0.3.12-rc.1 ant-cli-v0.3.12-rc.1
if [ "$rc" = 0 ] && gh_called && placeholder 0.3.12; then
  ok "empty generated notes get the placeholder"
else bad "empty generated notes get the placeholder (notes=$notes)"; fi

# 8. No previous stable release: placeholder, and the API is not called.
FAKE_TAGS="ant-cli-v0.1.1-rc.1" FAKE_GH_OUT="$generated_body" FAKE_GH_EXIT=0 run 0.1.1 ant-cli-v0.1.1
if [ "$rc" = 0 ] && ! gh_called && placeholder 0.1.1; then
  ok "no previous stable release gets the placeholder without an API call"
else bad "no previous stable release gets the placeholder without an API call (notes=$notes)"; fi

# 9. Bad arguments and a missing changelog exit 1.
rc=0; bash "$script" 0.3.12 ant-cli-v0.3.12 > /dev/null 2>&1 || rc=$?
if [ "$rc" = 1 ]; then ok "too few arguments exit 1"; else bad "too few arguments exit $rc"; fi
rc=0; bash "$script" 0.3.12 ant-cli-v0.3.12 "$work/notes.md" "$work/missing.md" > /dev/null 2>&1 || rc=$?
if [ "$rc" = 1 ]; then ok "a missing changelog exits 1"; else bad "a missing changelog exits $rc"; fi

echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
