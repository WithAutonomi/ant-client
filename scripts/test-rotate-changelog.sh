#!/usr/bin/env bash
# Tests for scripts/rotate-changelog.sh. CI runs this in the `Release scripts` job.
#
#   scripts/test-rotate-changelog.sh [<rotate-changelog.sh>]

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="${1:-$here/rotate-changelog.sh}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0 fail=0
ok() { echo "ok    $*"; pass=$((pass + 1)); }
bad() { echo "FAIL  $*"; fail=$((fail + 1)); }
mode() { stat -c %a "$1" 2> /dev/null || stat -f %Lp "$1"; }
inode() { stat -c %i "$1" 2> /dev/null || stat -f %i "$1"; }

# fresh <unreleased entries>: writes a changelog with those entries under [Unreleased].
fresh() {
  rm -rf "$work/repo"
  mkdir "$work/repo"
  {
    printf '# Changelog\n\n## [Unreleased]\n'
    [ -n "$1" ] && printf '\n%s\n' "$1"
    printf '\n## [0.3.12] - 2026-10-22\n\n### Fixed\n- An older fix (#232)\n'
  } > "$work/repo/CHANGELOG.md"
}
# rotate <args...>: runs the script on the test changelog; sets $rc and $log.
rotate() {
  rc=0
  log="$(bash "$script" "$@" "$work/repo/CHANGELOG.md" 2>&1)" || rc=$?
}
count() { grep -c -F -- "$1" "$work/repo/CHANGELOG.md" || true; }

entries=$'### Added\n- A new thing (#300)'

# 1. Rotation: [Unreleased] stays, empty, with the dated section and its entries under it.
fresh "$entries"
rotate 0.3.13 2026-11-05
expected=$'# Changelog\n\n## [Unreleased]\n\n## [0.3.13] - 2026-11-05\n\n### Added\n- A new thing (#300)\n\n## [0.3.12] - 2026-10-22\n\n### Fixed\n- An older fix (#232)'
if [ "$rc" = 0 ] && [ "$(cat "$work/repo/CHANGELOG.md")" = "$expected" ]; then
  ok "rotates [Unreleased] into a dated section"
else bad "rotates [Unreleased] into a dated section (rc=$rc): $(cat "$work/repo/CHANGELOG.md")"; fi

# 2. Re-running is a no-op.
cp "$work/repo/CHANGELOG.md" "$work/before.md"
rotate 0.3.13-rc.2 2026-11-06
if [ "$rc" = 0 ] && cmp -s "$work/before.md" "$work/repo/CHANGELOG.md" && printf '%s' "$log" | grep -q 'nothing to do'; then
  ok "a re-run leaves the changelog alone"
else bad "a re-run leaves the changelog alone (rc=$rc, log=$log)"; fi

# 3. Entries added after the rotation: exit 2, nothing changed.
fresh "$entries"
rotate 0.3.13 2026-11-05
awk '{ print } /^## \[Unreleased\]/ { print ""; print "### Fixed"; print "- A late fix (#301)" }' \
  "$work/repo/CHANGELOG.md" > "$work/late.md"
cp "$work/late.md" "$work/repo/CHANGELOG.md"
rotate 0.3.13 2026-11-05
if [ "$rc" = 2 ] && cmp -s "$work/late.md" "$work/repo/CHANGELOG.md"; then
  ok "entries on both sides exit 2 and change nothing"
else bad "entries on both sides exit 2 and change nothing (rc=$rc)"; fi

# 4. An rc version drops its suffix.
fresh "$entries"
rotate 0.3.13-rc.1 2026-11-05
if [ "$rc" = 0 ] && [ "$(count '## [0.3.13] - 2026-11-05')" = 1 ] && [ "$(count 'rc.1')" = 0 ]; then
  ok "an rc version writes the base version's heading"
else bad "an rc version writes the base version's heading (rc=$rc)"; fi

# 5. An empty hand-written heading is replaced, not duplicated.
fresh "$entries"
awk '{ print } /^## \[Unreleased\]/ { print ""; print "## [0.3.13]" }' \
  "$work/repo/CHANGELOG.md" > "$work/hand.md"
cp "$work/hand.md" "$work/repo/CHANGELOG.md"
rotate 0.3.13 2026-11-05
if [ "$rc" = 0 ] && [ "$(count '## [0.3.13]')" = 1 ] && [ "$(count '- A new thing (#300)')" = 1 ]; then
  ok "an empty hand-written heading is replaced"
else bad "an empty hand-written heading is replaced (rc=$rc): $(cat "$work/repo/CHANGELOG.md")"; fi

# 6. An empty [Unreleased] warns, exits 0 and changes nothing.
fresh ""
cp "$work/repo/CHANGELOG.md" "$work/before.md"
rotate 0.3.13 2026-11-05
if [ "$rc" = 0 ] && cmp -s "$work/before.md" "$work/repo/CHANGELOG.md" && printf '%s' "$log" | grep -q 'WARNING'; then
  ok "an empty [Unreleased] warns and changes nothing"
else bad "an empty [Unreleased] warns and changes nothing (rc=$rc, log=$log)"; fi

# 7. The changelog is replaced by a rename, keeps its mode, and no temporary file is left behind.
fresh "$entries"
chmod 640 "$work/repo/CHANGELOG.md"
inode_before="$(inode "$work/repo/CHANGELOG.md")"
rotate 0.3.13 2026-11-05
inode_after="$(inode "$work/repo/CHANGELOG.md")"
left="$(find "$work/repo" -mindepth 1 -not -name CHANGELOG.md | wc -l | tr -d ' ')"
if [ "$rc" = 0 ] && [ "$inode_before" != "$inode_after" ] && [ "$(mode "$work/repo/CHANGELOG.md")" = 640 ] && [ "$left" = 0 ]; then
  ok "the changelog is renamed into place with its mode kept"
else bad "the changelog is renamed into place with its mode kept (rc=$rc, inode $inode_before -> $inode_after, mode $(mode "$work/repo/CHANGELOG.md"), $left left over)"; fi

# 8. Bad input exits 1 and changes nothing.
fresh "$entries"
cp "$work/repo/CHANGELOG.md" "$work/before.md"
rotate 0.3 2026-11-05
if [ "$rc" = 1 ]; then ok "a bad version exits 1"; else bad "a bad version exits $rc"; fi
rotate 0.3.13 05/11/2026
if [ "$rc" = 1 ]; then ok "a bad date exits 1"; else bad "a bad date exits $rc"; fi
printf '# Changelog\n\n## [0.3.12] - 2026-10-22\n' > "$work/repo/CHANGELOG.md"
rotate 0.3.13 2026-11-05
if [ "$rc" = 1 ]; then ok "no [Unreleased] heading exits 1"; else bad "no [Unreleased] heading exits $rc"; fi

echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
